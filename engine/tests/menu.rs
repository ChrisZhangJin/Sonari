//! 右键菜单命令的端到端集成测试（走真实的 `dispatch_write` / `dispatch_read`）。
//!
//! 覆盖 ocenaudio 式右键菜单的后端链路：
//! 转换为单声道 / 转换为采样率 / 查看信息 / 保存 / 另存为（含格式转换）/ 重命名。

use std::f64::consts::PI;
use std::path::PathBuf;

use serde_json::{json, Value};
use sonari_engine::commands::{dispatch_read, dispatch_write, Docs};
use sonari_engine::decode::{decode_file, AudioBuffer};
use sonari_engine::encode::{write_wav, EncodeOptions};

fn tmp_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("sonari-menu-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 2 秒 立体声 44.1 kHz：左 440 Hz / 右 3000 Hz（16 位 WAV）。
/// 每个用例用独立文件名 —— 测试并行跑，共用同一路径会撞上 Windows 的写占用。
fn fixture(tag: &str) -> PathBuf {
    let rate = 44_100u32;
    let frames = rate as usize * 2;
    let mut samples = Vec::with_capacity(frames * 2);
    for f in 0..frames {
        let t = f as f64 / rate as f64;
        samples.push((2.0 * PI * 440.0 * t).sin() as f32 * 0.5);
        samples.push((2.0 * PI * 3000.0 * t).sin() as f32 * 0.5);
    }
    let buf = AudioBuffer {
        sample_rate: rate,
        channels: 2,
        samples,
    };
    let p = tmp_dir().join(format!("fixture-{tag}.wav"));
    write_wav(&p, &buf, 16).unwrap();
    p
}

fn open(docs: &mut Docs, path: &PathBuf) -> Value {
    dispatch_write(docs, "doc_open", &json!({ "path": path.to_string_lossy() })).unwrap()
}

fn f(v: &Value, k: &str) -> f64 {
    v["info"][k].as_f64().unwrap_or(f64::NAN)
}

#[test]
fn convert_channels_keeps_frames_and_downmixes() {
    let mut docs = Docs::new();
    let src = fixture("channels");
    let opened = open(&mut docs, &src);
    let id = opened["docId"].as_str().unwrap().to_string();
    assert_eq!(opened["info"]["channels"], 2);
    assert_eq!(f(&opened, "frames"), 88_200.0);

    let mono = dispatch_write(&mut docs, "doc_channels", &json!({ "docId": id, "channels": 1 })).unwrap();
    assert_eq!(mono["info"]["channels"], 1);
    assert_eq!(mono["info"]["channelsText"], "单声道 (Mono)");
    // 声道数变化不改帧数与采样率
    assert_eq!(f(&mono, "frames"), 88_200.0);
    assert_eq!(f(&mono, "sampleRate"), 44_100.0);
    assert!((f(&mono, "durationSec") - 2.0).abs() < 1e-6);
    // 峰值缓存随转换刷新（4096 桶 × 2 值）
    assert_eq!(mono["peaks"].as_array().unwrap().len(), 4096 * 2);

    // 已是单声道 → 拒绝并在消息里说清楚
    let again = dispatch_write(&mut docs, "doc_channels", &json!({ "docId": id, "channels": 1 }));
    assert!(again.is_err());
    assert!(again.unwrap_err().contains("已是单声道"));

    // 再上混回立体声
    let st = dispatch_write(&mut docs, "doc_channels", &json!({ "docId": id, "channels": 2 })).unwrap();
    assert_eq!(st["info"]["channels"], 2);
    assert_eq!(st["info"]["channelsText"], "双声道 (Stereo)");

    let _ = std::fs::remove_file(&src);
}

#[test]
fn resample_scales_frames_and_keeps_duration() {
    let mut docs = Docs::new();
    let src = fixture("resample");
    let opened = open(&mut docs, &src);
    let id = opened["docId"].as_str().unwrap().to_string();

    // 44.1k → 16k：帧数 88200 × (16000/44100) = 32000
    let down = dispatch_write(&mut docs, "doc_resample", &json!({ "docId": id, "sampleRate": 16000 })).unwrap();
    assert_eq!(f(&down, "sampleRate"), 16_000.0);
    assert_eq!(f(&down, "frames"), 32_000.0);
    assert!((f(&down, "durationSec") - 2.0).abs() < 1e-3);

    // 16k → 8k
    let d8 = dispatch_write(&mut docs, "doc_resample", &json!({ "docId": id, "sampleRate": 8000 })).unwrap();
    assert_eq!(f(&d8, "sampleRate"), 8000.0);
    assert_eq!(f(&d8, "frames"), 16_000.0);

    // 越界采样率被拒
    let bad = dispatch_write(&mut docs, "doc_resample", &json!({ "docId": id, "sampleRate": 100 }));
    assert!(bad.is_err());

    // 同采样率无需转换
    let same = dispatch_write(&mut docs, "doc_resample", &json!({ "docId": id, "sampleRate": 8000 }));
    assert!(same.unwrap_err().contains("无需转换"));

    let _ = std::fs::remove_file(&src);
}

#[test]
fn save_as_converts_format_and_rebinds_document() {
    let mut docs = Docs::new();
    let src = fixture("saveas");
    let opened = open(&mut docs, &src);
    let id = opened["docId"].as_str().unwrap().to_string();

    // 声道 + 采样率一起转换，再另存为 24 位 WAV
    dispatch_write(&mut docs, "doc_channels", &json!({ "docId": id, "channels": 1 })).unwrap();
    dispatch_write(&mut docs, "doc_resample", &json!({ "docId": id, "sampleRate": 22050 })).unwrap();

    let out = tmp_dir().join("out24.wav");
    let r = dispatch_write(
        &mut docs,
        "doc_save",
        &json!({ "docId": id, "path": out.to_string_lossy(), "format": "wav", "bitDepth": 24 }),
    )
    .unwrap();
    assert_eq!(r["format"], "wav");
    assert_eq!(r["bitDepth"], 24);
    assert_eq!(r["viaFfmpeg"], false);
    assert_eq!(r["rebound"], true, "另存为应把文档改绑到新文件");
    assert!(out.is_file());

    // 回读：采样率/声道/时长都对得上
    let back = decode_file(&out).unwrap();
    assert_eq!(back.sample_rate, 22_050);
    assert_eq!(back.channels, 1);
    assert_eq!(back.frames(), 44_100);

    // 文档已改绑 → doc_info 现在描述新文件
    let info = dispatch_read(&docs, "doc_info", &json!({ "docId": id })).unwrap();
    assert_eq!(info["name"], "out24.wav");
    assert_eq!(info["path"], out.to_string_lossy().to_string());
    assert_eq!(info["container"], "WAV");
    assert_eq!(info["bitsPerSample"], 24);
    assert_eq!(info["sampleRate"], 22_050);
    assert_eq!(info["channelsText"], "单声道 (Mono)");

    // 「保存」不带 path = 覆盖写回当前文件，不改绑
    let r2 = dispatch_write(&mut docs, "doc_save", &json!({ "docId": id })).unwrap();
    assert_eq!(r2["rebound"], false);
    assert_eq!(r2["path"], out.to_string_lossy().to_string());

    // 「导出…」只写选区：前 0.5 秒
    let part = tmp_dir().join("part.wav");
    let r3 = dispatch_write(
        &mut docs,
        "doc_save",
        &json!({ "docId": id, "path": part.to_string_lossy(), "format": "wav",
                 "begin": 0, "end": 11_025 }),
    )
    .unwrap();
    assert_eq!(r3["region"], true);
    assert_eq!(r3["frames"], 11_025);
    let pback = decode_file(&part).unwrap();
    assert!((pback.duration_secs() - 0.5).abs() < 1e-6);

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&part);
}

#[test]
fn save_as_via_ffmpeg_when_available() {
    let caps = dispatch_read(&Docs::new(), "encoder_caps", &json!({})).unwrap();
    let mp3_available = caps["formats"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["ext"] == "mp3" && f["available"] == true);
    if !mp3_available {
        eprintln!("跳过：未检测到 ffmpeg");
        return;
    }

    let mut docs = Docs::new();
    let src = fixture("ffmpeg");
    let opened = open(&mut docs, &src);
    let id = opened["docId"].as_str().unwrap().to_string();

    let mp3 = tmp_dir().join("out.mp3");
    let r = dispatch_write(
        &mut docs,
        "doc_save",
        &json!({ "docId": id, "path": mp3.to_string_lossy(), "format": "mp3", "bitrateKbps": 192 }),
    )
    .unwrap();
    assert_eq!(r["viaFfmpeg"], true);
    assert!(r["bytes"].as_u64().unwrap() > 1000, "mp3 应该有几 KB");

    // 必须能被自家解码器读回来（= ffmpeg 真的写出了有效 MP3）
    let back = decode_file(&mp3).unwrap();
    assert_eq!(back.sample_rate, 44_100);
    assert_eq!(back.channels, 2);
    assert!(
        (back.duration_secs() - 2.0).abs() < 0.15,
        "时长 {} 应接近 2 秒",
        back.duration_secs()
    );

    // doc_info 应反映转码后的真实格式（MP3，非 PCM）
    let info = dispatch_read(&docs, "doc_info", &json!({ "docId": id })).unwrap();
    assert_eq!(info["container"], "MP3");
    assert!(info["codecLabel"].as_str().unwrap().contains("MP3"), "{}", info["codecLabel"]);

    // FLAC 无损往返
    let flac = tmp_dir().join("out.flac");
    let rf = dispatch_write(
        &mut docs,
        "doc_save",
        &json!({ "docId": id, "path": flac.to_string_lossy(), "format": "flac" }),
    )
    .unwrap();
    assert_eq!(rf["viaFfmpeg"], true);
    let fback = decode_file(&flac).unwrap();
    assert_eq!(fback.sample_rate, 44_100);
    assert!((fback.duration_secs() - 2.0).abs() < 0.05, "flac 时长 {}", fback.duration_secs());
    // 无损：与源样本逐点接近（原始 0.5 幅度正弦）
    let peak = fback.samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak - 0.5).abs() < 0.01, "flac 峰值 {peak} 应保持 0.5");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&mp3);
    let _ = std::fs::remove_file(&flac);
}

#[test]
fn rename_moves_file_on_disk() {
    let mut docs = Docs::new();
    let src = tmp_dir().join("before.wav");
    let buf = AudioBuffer {
        sample_rate: 8000,
        channels: 1,
        samples: vec![0.1; 800],
    };
    write_wav(&src, &buf, 16).unwrap();

    let opened = open(&mut docs, &src);
    let id = opened["docId"].as_str().unwrap().to_string();

    let r = dispatch_write(&mut docs, "doc_rename", &json!({ "docId": id, "name": "after.wav" })).unwrap();
    assert_eq!(r["name"], "after.wav");
    let after = tmp_dir().join("after.wav");
    assert!(after.is_file(), "磁盘文件应已改名");
    assert!(!src.exists());

    // 非法字符被拒
    let bad = dispatch_write(&mut docs, "doc_rename", &json!({ "docId": id, "name": "a/b.wav" }));
    assert!(bad.is_err());
    // 空名被拒
    assert!(dispatch_write(&mut docs, "doc_rename", &json!({ "docId": id, "name": "  " })).is_err());

    let _ = std::fs::remove_file(&after);
}

#[test]
fn memory_document_has_no_path_and_needs_save_as() {
    let mut docs = Docs::new();
    // 模拟录音产物：不走磁盘，直接内存入库
    let samples: Vec<f32> = (0..8000).map(|i| (i as f32 / 8000.0) * 0.2).collect();
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for s in &samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    };
    let opened = dispatch_write(
        &mut docs,
        "doc_open_data",
        &json!({ "name": "录音", "sampleRate": 8000, "channels": 1, "f32leBase64": b64 }),
    )
    .unwrap();
    let id = opened["docId"].as_str().unwrap().to_string();
    assert_eq!(opened["info"]["bitsPerSample"], 32);

    // 保存 → 拒绝，并指向另存为
    let e = dispatch_write(&mut docs, "doc_save", &json!({ "docId": id })).unwrap_err();
    assert!(e.contains("另存为"), "{e}");

    // 重命名 → 只改显示名，不报错
    let r = dispatch_write(&mut docs, "doc_rename", &json!({ "docId": id, "name": "我的录音" })).unwrap();
    assert_eq!(r["name"], "我的录音");
    assert_eq!(r["path"], "");
}

#[test]
fn encoder_caps_reports_wav_as_builtin() {
    let caps = dispatch_read(&Docs::new(), "encoder_caps", &json!({})).unwrap();
    let formats = caps["formats"].as_array().unwrap();
    let wav = formats.iter().find(|f| f["ext"] == "wav").unwrap();
    assert_eq!(wav["builtin"], true);
    // 内建格式没有 available 字段（永远可用），ffmpeg 格式才有
    assert!(wav.get("available").is_none() || wav["available"] == true);
    assert_eq!(caps["bitDepths"], json!([16, 24, 32]));
}

/// 回归：`doc_open` 必须回传 path —— 前端「保存 / 导出 / 另存为 / 重命名」
/// 全靠它判断文档是否已关联磁盘文件，缺了会静默退化成「另存为」。
#[test]
fn doc_open_reports_path_for_file_operations() {
    let mut docs = Docs::new();
    let src = fixture("path");
    let opened = open(&mut docs, &src);
    assert_eq!(opened["path"], src.to_string_lossy().to_string());
    assert_eq!(
        opened["name"],
        src.file_name().unwrap().to_string_lossy().to_string()
    );
    // 转换后 path 必须保留（否则「保存」会失效）
    let id = opened["docId"].as_str().unwrap().to_string();
    let after = dispatch_write(&mut docs, "doc_resample", &json!({ "docId": id, "sampleRate": 32000 })).unwrap();
    assert_eq!(after["path"], src.to_string_lossy().to_string());
    let _ = std::fs::remove_file(&src);
}

#[test]
fn unknown_command_is_rejected() {
    let mut docs = Docs::new();
    assert!(dispatch_read(&docs, "nope", &json!({})).is_err());
    assert!(dispatch_write(&mut docs, "nope", &json!({})).is_err());
}

#[test]
fn save_options_defaults_are_sane() {
    let o = EncodeOptions::from_params(&json!({ "bitDepth": 24, "format": "WAV" }));
    assert_eq!(o.format, "wav");
    assert_eq!(o.bit_depth, 24);
}
