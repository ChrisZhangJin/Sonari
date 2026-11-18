//! 编辑命令集成测试：剪切 / 复制 / 粘贴 / 静音 + 撤销 / 重做历史。

use base64::Engine as _;
use serde_json::{json, Value};
use sonari_engine::commands::{dispatch_read, dispatch_write, Docs};

/// 内存文档：单声道 1000 帧，样本值 = 帧号（便于核对位置）
fn open_ramp(docs: &mut Docs) -> String {
    let mut bytes = Vec::new();
    for i in 0..1000 {
        bytes.extend_from_slice(&(i as f32).to_le_bytes());
    }
    let r = dispatch_write(
        docs,
        "doc_open_data",
        &json!({
            "name": "ramp",
            "sampleRate": 1000,
            "channels": 1,
            "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
    )
    .unwrap();
    r["docId"].as_str().unwrap().to_string()
}

fn samples(docs: &Docs, id: &str) -> Vec<f32> {
    let r = dispatch_read(docs, "pcm", &json!({ "docId": id })).unwrap();
    let b = base64::engine::general_purpose::STANDARD
        .decode(r["f32leBase64"].as_str().unwrap())
        .unwrap();
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn undo_labels(v: &Value) -> Vec<String> {
    v["history"]["undo"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn cut_paste_silence_undo_redo() {
    let mut docs = Docs::new();
    let id = open_ramp(&mut docs);

    // 剪切 [100, 200)
    let r = dispatch_write(&mut docs, "doc_cut", &json!({ "docId": id, "begin": 100, "end": 200 })).unwrap();
    assert_eq!(r["info"]["frames"], 900);
    let s = samples(&docs, &id);
    assert_eq!(s[99], 99.0);
    assert_eq!(s[100], 200.0);
    let clip = dispatch_read(&docs, "clipboard_info", &json!({})).unwrap();
    assert_eq!(clip["frames"], 100);

    // 插入粘贴到 0
    let r = dispatch_write(&mut docs, "doc_paste_clip", &json!({ "docId": id, "frame": 0 })).unwrap();
    assert_eq!(r["info"]["frames"], 1000);
    let s = samples(&docs, &id);
    assert_eq!(s[0], 100.0);
    assert_eq!(s[99], 199.0);
    assert_eq!(s[100], 0.0);

    // 替换粘贴：[0, 50) 换成剪贴板 100 帧 → 1050 帧
    let r = dispatch_write(
        &mut docs,
        "doc_paste_clip",
        &json!({ "docId": id, "begin": 0, "end": 50 }),
    )
    .unwrap();
    assert_eq!(r["info"]["frames"], 1050);

    // 静音 [0, 10)
    let r = dispatch_write(&mut docs, "doc_silence", &json!({ "docId": id, "begin": 0, "end": 10 })).unwrap();
    assert!(samples(&docs, &id)[..10].iter().all(|v| *v == 0.0));
    assert_eq!(undo_labels(&r).len(), 4);

    // 撤销一步：静音没了
    let r = dispatch_write(&mut docs, "doc_undo", &json!({ "docId": id })).unwrap();
    assert_eq!(samples(&docs, &id)[0], 100.0);
    assert_eq!(r["history"]["redo"].as_array().unwrap().len(), 1);

    // 再撤销三步 → 回到原始
    let r = dispatch_write(&mut docs, "doc_undo", &json!({ "docId": id, "steps": 3 })).unwrap();
    assert_eq!(r["info"]["frames"], 1000);
    assert_eq!(samples(&docs, &id), (0..1000).map(|i| i as f32).collect::<Vec<_>>());
    assert!(undo_labels(&r).is_empty());
    assert!(dispatch_write(&mut docs, "doc_undo", &json!({ "docId": id })).is_err());

    // 重做一步 → 剪切后的状态
    let r = dispatch_write(&mut docs, "doc_redo", &json!({ "docId": id })).unwrap();
    assert_eq!(r["info"]["frames"], 900);

    // 新操作清空 redo
    let r = dispatch_write(&mut docs, "doc_silence", &json!({ "docId": id, "begin": 0, "end": 1 })).unwrap();
    assert!(r["history"]["redo"].as_array().unwrap().is_empty());
}

#[test]
fn copy_keeps_doc_and_cannot_cut_everything() {
    let mut docs = Docs::new();
    let id = open_ramp(&mut docs);
    dispatch_write(&mut docs, "doc_copy", &json!({ "docId": id, "begin": 0, "end": 10 })).unwrap();
    assert_eq!(samples(&docs, &id).len(), 1000);
    assert!(dispatch_write(&mut docs, "doc_cut", &json!({ "docId": id, "begin": 0, "end": 1000 })).is_err());
    assert!(dispatch_write(&mut docs, "doc_silence", &json!({ "docId": id, "begin": 5, "end": 5 })).is_err());
}

#[test]
fn fade_in_and_out_ramp_linearly() {
    let mut docs = Docs::new();
    let id = open_ramp(&mut docs); // 样本值 = 帧号
    // 淡入 [100, 201)：首帧 ×0，末帧 ×1，中点 ×0.5
    dispatch_write(&mut docs, "doc_fade", &json!({ "docId": id, "begin": 100, "end": 201, "dir": "in" })).unwrap();
    let s = samples(&docs, &id);
    assert_eq!(s[99], 99.0);
    assert_eq!(s[100], 0.0);
    assert!((s[150] - 150.0 * 0.5).abs() < 1e-3);
    assert!((s[200] - 200.0).abs() < 1e-3);
    // 淡出 [900, 1000)：末帧 → 0
    let r = dispatch_write(&mut docs, "doc_fade", &json!({ "docId": id, "begin": 900, "end": 1000, "dir": "out" })).unwrap();
    let s = samples(&docs, &id);
    assert!((s[900] - 900.0).abs() < 1e-3);
    assert_eq!(s[999], 0.0);
    assert_eq!(r["history"]["undo"].as_array().unwrap().len(), 2);
    assert!(dispatch_write(&mut docs, "doc_fade", &json!({ "docId": id, "begin": 0, "end": 10, "dir": "up" })).is_err());
}

/// 粘贴到「采样率 / 声道不同」的文档 —— 应自动适配后插入
#[test]
fn paste_clip_adapts_rate_and_channels() {
    let mut docs = Docs::new();
    // 源：单声道 1000 Hz，1000 帧
    let id = open_ramp(&mut docs);
    // 剪下 [0, 100) → 剪贴板是 1000 Hz 单声道
    dispatch_write(&mut docs, "doc_copy", &json!({ "docId": id, "begin": 0, "end": 100 })).unwrap();

    // 新建一个 2000 Hz 双声道文档
    let mut bytes = Vec::new();
    for _ in 0..500 {
        for _ in 0..2 {
            bytes.extend_from_slice(&0.5f32.to_le_bytes());
        }
    }
    let r = dispatch_write(
        &mut docs,
        "doc_open_data",
        &json!({
            "name": "stereo2k",
            "sampleRate": 2000,
            "channels": 2,
            "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
    )
    .unwrap();
    let id2 = r["docId"].as_str().unwrap().to_string();

    // 粘贴：剪贴板 1000Hz 单声道 → 目标 2000Hz 双声道
    // 采样率翻倍 → 200 帧；声道翻倍 → 每帧两样本 → 共 400 个样本
    let r = dispatch_write(&mut docs, "doc_paste_clip", &json!({ "docId": id2, "frame": 0 })).unwrap();
    assert_eq!(r["info"]["channels"], 2);
    assert_eq!(r["info"]["sampleRate"], 2000);
    // 500 原帧 + 200 插入帧 = 700 帧
    assert_eq!(r["info"]["frames"], 700);
    assert_eq!(r["info"]["channels"], 2);
}

/// 「转换为采样率 / 声道」现在走 commit() → 应产生可撤销的历史条目
#[test]
fn resample_and_channels_are_undoable() {
    let mut docs = Docs::new();
    let id = open_ramp(&mut docs);

    let r = dispatch_write(&mut docs, "doc_resample", &json!({ "docId": id, "sampleRate": 4000 })).unwrap();
    assert_eq!(r["info"]["sampleRate"], 4000);
    assert_eq!(r["info"]["frames"], 4000);
    assert_eq!(undo_labels(&r).len(), 1, "重采样应产生 1 条撤销历史");

    let r = dispatch_write(&mut docs, "doc_channels", &json!({ "docId": id, "channels": 2 })).unwrap();
    assert_eq!(r["info"]["channels"], 2);
    assert_eq!(undo_labels(&r).len(), 2, "转换声道应产生第 2 条撤销历史");

    // 撤销两步 → 回到原始单声道 1000 Hz / 1000 帧
    let r = dispatch_write(&mut docs, "doc_undo", &json!({ "docId": id, "steps": 2 })).unwrap();
    assert_eq!(r["info"]["channels"], 1);
    assert_eq!(r["info"]["sampleRate"], 1000);
    assert_eq!(r["info"]["frames"], 1000);
    assert_eq!(samples(&docs, &id), (0..1000).map(|i| i as f32).collect::<Vec<_>>());
}

/// 星座图指纹：短文件应产出峰与哈希；时间窗参数应被接受
#[test]
fn mss_constellation_produces_peaks_and_hashes() {
    let mut docs = Docs::new();
    // 1 秒 440Hz 正弦 @ 8000 Hz（mss 内部重采样目标就是 8k）
    let sr = 8000usize;
    let mut bytes = Vec::new();
    for i in 0..sr {
        let v = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / sr as f64).sin() as f32 * 0.5;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let r = dispatch_write(
        &mut docs,
        "doc_open_data",
        &json!({
            "name": "tone440",
            "sampleRate": sr,
            "channels": 1,
            "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
    )
    .unwrap();
    let id = r["docId"].as_str().unwrap().to_string();

    let m = dispatch_read(&docs, "mss_constellation", &json!({ "docId": id })).unwrap();
    assert!(m["frames"].as_u64().unwrap() > 50, "应产出足够的帧");
    let peaks = m["peaks"].as_array().unwrap().len();
    let hashes = m["hashes"].as_array().unwrap().len();
    assert!(peaks > 0, "应产出峰");
    assert!(hashes > 0, "应产出哈希");
    assert!(peaks % 3 == 0 && hashes % 4 == 0, "峰按 3 元组、哈希按 4 元组平铺");
    assert_eq!(m["beginSec"], 0.0);

    // 带窗口参数：只算 [0.25, 0.75) 秒
    let m2 = dispatch_read(
        &docs,
        "mss_constellation",
        &json!({ "docId": id, "begin": 0.25, "end": 0.75 }),
    )
    .unwrap();
    assert_eq!(m2["beginSec"], 0.25);
    assert_eq!(m2["endSec"], 0.75);
    assert!(m2["frames"].as_u64().unwrap() < m["frames"].as_u64().unwrap(), "窗口内帧数应更少");

    // 非法窗口 → 报错
    assert!(dispatch_read(&docs, "mss_constellation", &json!({ "docId": id, "begin": 0.5, "end": 0.5 })).is_err());
}

#[test]
fn fade_follows_custom_curve() {
    let mut docs = Docs::new();
    let id = open_ramp(&mut docs); // 样本值 = 帧号
    // 曲线 [0, 1, 0.5]：区间 [0, 101) 前半 0→1，后半 1→0.5
    let r = dispatch_write(
        &mut docs,
        "doc_fade",
        &json!({ "docId": id, "begin": 0, "end": 101, "dir": "in", "curve": [0.0, 1.0, 0.5] }),
    )
    .unwrap();
    let s = samples(&docs, &id);
    assert_eq!(s[0], 0.0);
    assert!((s[25] - 25.0 * 0.5).abs() < 1e-3); // x=0.25 → 0.5
    assert!((s[50] - 50.0).abs() < 1e-3); // x=0.5 → 1
    assert!((s[100] - 100.0 * 0.5).abs() < 1e-3); // x=1 → 0.5
    assert_eq!(s[101], 101.0); // 区间外不变
    assert!(r["history"]["undo"][0].as_str().unwrap().contains("曲线"));
    // 非法曲线
    assert!(dispatch_write(&mut docs, "doc_fade", &json!({ "docId": id, "begin": 0, "end": 10, "dir": "in", "curve": [1.0] })).is_err());
    assert!(dispatch_write(&mut docs, "doc_fade", &json!({ "docId": id, "begin": 0, "end": 10, "dir": "in", "curve": ["x", 1] })).is_err());
}

/// 立体声文档：左 = 帧号，右 = 帧号的负数
fn open_stereo(docs: &mut Docs, frames: usize, sr: u32, l: impl Fn(usize) -> f32, r: impl Fn(usize) -> f32) -> String {
    let mut bytes = Vec::new();
    for i in 0..frames {
        bytes.extend_from_slice(&l(i).to_le_bytes());
        bytes.extend_from_slice(&r(i).to_le_bytes());
    }
    dispatch_write(
        docs,
        "doc_open_data",
        &json!({ "name": "call.mp3", "sampleRate": sr, "channels": 2,
                 "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes) }),
    )
    .unwrap()["docId"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn extract_channel_creates_new_mono_doc() {
    let mut docs = Docs::new();
    let id = open_stereo(&mut docs, 1000, 1000, |i| i as f32, |i| -(i as f32));
    // 整个右声道
    let r = dispatch_write(&mut docs, "doc_extract_channel", &json!({ "docId": id, "channel": 1 })).unwrap();
    let nid = r["docId"].as_str().unwrap().to_string();
    assert_ne!(nid, id);
    assert_eq!(r["name"], "call_R.wav");
    assert_eq!(r["info"]["channels"], 1);
    assert_eq!(r["info"]["frames"], 1000);
    let s = samples(&docs, &nid);
    assert_eq!(s[0], 0.0);
    assert_eq!(s[999], -999.0);
    // 左声道的区间 [100, 300)
    let r = dispatch_write(
        &mut docs,
        "doc_extract_channel",
        &json!({ "docId": id, "channel": 0, "begin": 100, "end": 300 }),
    )
    .unwrap();
    assert_eq!(r["name"], "call_L_0.10-0.30s.wav");
    let s = samples(&docs, r["docId"].as_str().unwrap());
    assert_eq!(s.len(), 200);
    assert_eq!(s[0], 100.0);
    assert_eq!(s[199], 299.0);
    // 原文档不变、越界声道报错
    assert_eq!(samples(&docs, &id).len(), 2000);
    assert!(dispatch_write(&mut docs, "doc_extract_channel", &json!({ "docId": id, "channel": 2 })).is_err());
}

/// 立体声星座图：必须覆盖整段（不能只算前一半样本），且按声道分别计算
#[test]
fn mss_constellation_per_channel_covers_whole_stereo_file() {
    let mut docs = Docs::new();
    let sr = 8000u32;
    let frames = 4 * sr as usize; // 4 s
    let tone = |i: usize| {
        let t = i as f32 / sr as f32;
        let f = [440.0, 880.0, 1320.0, 660.0][(t / 0.25) as usize % 4];
        (2.0 * std::f32::consts::PI * f * t).sin() * 0.5
    };
    // 左声道只有前 1 s，右声道只有最后 1 s 有声音
    let id = open_stereo(
        &mut docs,
        frames,
        sr,
        |i| if i < sr as usize { tone(i) } else { 0.0 },
        |i| if i >= 3 * sr as usize { tone(i) } else { 0.0 },
    );
    let max_peak_sec = |m: &Value| -> f64 {
        let p = m["peaks"].as_array().unwrap();
        (0..p.len()).step_by(3).map(|k| p[k].as_f64().unwrap()).fold(-1.0, f64::max) * 0.01
    };
    let get = |c: Option<u64>| {
        let mut p = json!({ "docId": id });
        if let Some(c) = c {
            p["channel"] = json!(c);
        }
        dispatch_read(&docs, "mss_constellation", &p).unwrap()
    };
    let mix = get(None);
    let left = get(Some(0));
    let right = get(Some(1));
    assert_eq!(left["channel"], 0);
    // 混合：最后 1 s 的右声道也要出现（旧实现只取了前一半样本，到不了 3 s 以后）
    assert!(max_peak_sec(&mix) > 3.5, "mix max {}", max_peak_sec(&mix));
    assert!(max_peak_sec(&left) < 1.1, "left max {}", max_peak_sec(&left));
    assert!(max_peak_sec(&right) > 3.5, "right max {}", max_peak_sec(&right));
    let first_right = right["peaks"][0].as_f64().unwrap() * 0.01;
    assert!(first_right > 2.9, "right first {first_right}");
    assert!(dispatch_read(&docs, "mss_constellation", &json!({ "docId": id, "channel": 2 })).is_err());
}
