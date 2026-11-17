//! 频谱图显示精度：绝对电平（dBFS）、窗增益补偿、缩放一致性。

use base64::Engine as _;
use serde_json::{json, Value};
use sonari_engine::commands::{dispatch_read, dispatch_write, Docs};

const SR: usize = 8000;

fn open(docs: &mut Docs, samples: &[f32]) -> String {
    let mut bytes = Vec::new();
    for v in samples {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let r = dispatch_write(
        docs,
        "doc_open_data",
        &json!({
            "name": "spec",
            "sampleRate": SR,
            "channels": 1,
            "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
    )
    .unwrap();
    r["docId"].as_str().unwrap().to_string()
}

fn frames(r: &Value) -> Vec<Vec<f64>> {
    r["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect())
        .collect()
}

fn frame_max(f: &[f64]) -> f64 {
    f.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
}

#[test]
fn full_scale_sine_reads_zero_dbfs() {
    let fft = 256usize;
    // 1000 Hz @ 8 kHz、fft 256 → bin 32 正好落在 bin 中心
    let bin = 32usize;
    let f0 = bin as f64 * SR as f64 / fft as f64;
    let x: Vec<f32> = (0..SR)
        .map(|i| (std::f64::consts::TAU * f0 * i as f64 / SR as f64).sin() as f32)
        .collect();
    let mut docs = Docs::new();
    let id = open(&mut docs, &x);
    let r = dispatch_read(&docs, "spectrogram", &json!({ "docId": id, "fft": fft, "hop": 64, "channel": 0 })).unwrap();
    assert_eq!(r["normalized"], false);
    let fr = frames(&r);
    assert!(!fr.is_empty());
    for f in &fr {
        assert!((f[bin] - 0.0).abs() <= 1.0, "bin {} = {} dB", bin, f[bin]);
        assert!((frame_max(f) - f[bin]).abs() < 1e-6);
    }
    assert!(r["maxDb"].as_f64().unwrap().abs() <= 1.0);

    // 半幅正弦 ≈ −6 dB（绝对电平，不是逐帧归一化的 0 dB）
    let y: Vec<f32> = x.iter().map(|v| v * 0.5).collect();
    let id2 = open(&mut docs, &y);
    let r2 = dispatch_read(&docs, "spectrogram", &json!({ "docId": id2, "fft": fft, "hop": 64 })).unwrap();
    for f in frames(&r2) {
        assert!((f[bin] + 6.02).abs() <= 1.0, "half-scale bin = {} dB", f[bin]);
    }
}

#[test]
fn quiet_segment_is_40db_below_loud_segment() {
    let fft = 256usize;
    let f0 = 700.0f64;
    let seg = SR; // 各 1 s
    let x: Vec<f32> = (0..2 * seg)
        .map(|i| {
            let a = if i < seg { 0.8 } else { 0.008 }; // −40 dB
            (a * (std::f64::consts::TAU * f0 * i as f64 / SR as f64).sin()) as f32
        })
        .collect();
    let mut docs = Docs::new();
    let id = open(&mut docs, &x);
    let hop = 64usize;
    let r = dispatch_read(
        &docs,
        "spectrogram",
        &json!({ "docId": id, "fft": fft, "hop": hop, "dynRange": 80, "maxFrames": 100000 }),
    )
    .unwrap();
    assert_eq!(r["hop"].as_u64().unwrap() as usize, hop);
    let fr = frames(&r);
    let mut loud = Vec::new();
    let mut quiet = Vec::new();
    for (k, f) in fr.iter().enumerate() {
        let s = k * hop;
        if s + fft <= seg {
            loud.push(frame_max(f));
        } else if s >= seg {
            quiet.push(frame_max(f));
        }
    }
    assert!(!loud.is_empty() && !quiet.is_empty());
    let l = loud.iter().sum::<f64>() / loud.len() as f64;
    let q = quiet.iter().sum::<f64>() / quiet.len() as f64;
    assert!(l > -3.0, "loud frames max {l} dB");
    assert!(((l - q) - 40.0).abs() <= 3.0, "loud {l} dB, quiet {q} dB");
    for v in &quiet {
        assert!(*v < -30.0, "quiet frame max {v} dB must not be normalised to 0 dB");
    }
    // 动态范围裁剪：最低值 = maxDb − dynRange
    let max_db = r["maxDb"].as_f64().unwrap();
    let floor = r["floorDb"].as_f64().unwrap();
    assert!((floor - (max_db - 80.0)).abs() < 1e-3);
    for f in &fr {
        for v in f {
            assert!(*v >= floor - 1e-3);
        }
    }
}

#[test]
fn zoomed_request_matches_full_request() {
    // 确定性的宽带「噪声」+ 两个音调，保证每帧都远高于裁剪底
    let mut seed = 12345u32;
    let x: Vec<f32> = (0..3 * SR)
        .map(|i| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            let t = i as f32 / SR as f32;
            0.1 * n + 0.3 * (std::f32::consts::TAU * 440.0 * t).sin() + 0.1 * (std::f32::consts::TAU * 2100.0 * t).sin()
        })
        .collect();
    let mut docs = Docs::new();
    let id = open(&mut docs, &x);
    let (fft, hop) = (256usize, 32usize);
    let full = dispatch_read(
        &docs,
        "spectrogram",
        &json!({ "docId": id, "fft": fft, "hop": hop, "maxFrames": 100000, "dynRange": 200 }),
    )
    .unwrap();
    let ref_db = full["maxDb"].as_f64().unwrap();
    let ff = frames(&full);
    // 窗口起点对齐 hop：缩放第 k 帧 = 全文件第 begin/hop + k 帧
    let begin = 300 * hop;
    let end = begin + 4000;
    let zoom = dispatch_read(
        &docs,
        "spectrogram",
        &json!({ "docId": id, "fft": fft, "hop": hop, "begin": begin, "end": end,
                 "maxFrames": 100000, "dynRange": 200, "refDb": ref_db }),
    )
    .unwrap();
    assert_eq!(zoom["hop"].as_u64().unwrap() as usize, hop);
    assert_eq!(zoom["begin"].as_u64().unwrap() as usize, begin);
    let zf = frames(&zoom);
    assert!(zf.len() > 50);
    let off = begin / hop;
    for (k, z) in zf.iter().enumerate() {
        let f = &ff[off + k];
        assert_eq!(z.len(), f.len());
        for (a, b) in z.iter().zip(f.iter()) {
            assert!((a - b).abs() < 0.01, "frame {k}: zoom {a} vs full {b}");
        }
    }
    assert!((zoom["floorDb"].as_f64().unwrap() - (ref_db - 200.0)).abs() < 1e-3);
}

#[test]
fn normalize_flag_keeps_legacy_per_frame_peak() {
    let x: Vec<f32> = (0..SR).map(|i| 0.01 * (i as f32 * 0.3).sin()).collect();
    let mut docs = Docs::new();
    let id = open(&mut docs, &x);
    let r = dispatch_read(&docs, "spectrogram", &json!({ "docId": id, "fft": 256, "hop": 64, "normalize": true })).unwrap();
    assert_eq!(r["normalized"], true);
    for f in frames(&r) {
        assert!(frame_max(&f).abs() < 1e-4);
    }
}
