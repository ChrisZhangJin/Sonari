//! 「查看丢包率」：没有真实包信息时按电平骤降估算。

use base64::Engine as _;
use serde_json::{json, Value};
use sonari_engine::commands::{dispatch_read, dispatch_write, Docs};

const SR: usize = 8000;
const PKT: usize = SR / 50; // 20 ms

fn open(docs: &mut Docs, samples: &[f32]) -> String {
    let mut bytes = Vec::new();
    for v in samples {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let r = dispatch_write(
        docs,
        "doc_open_data",
        &json!({
            "name": "call",
            "sampleRate": SR,
            "channels": 1,
            "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
    )
    .unwrap();
    r["docId"].as_str().unwrap().to_string()
}

/// 不含 0 样本的「语音」（直流偏置的正弦，始终 > 0）
fn tone(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.3 + 0.2 * (i as f32 * 0.35).sin()).collect()
}

fn loss(docs: &Docs, id: &str) -> Value {
    dispatch_read(docs, "doc_packet_loss", &json!({ "docId": id })).unwrap()
}

#[test]
fn estimates_loss_from_dropout_gaps() {
    let mut y = vec![0.0; SR / 10]; // 前导 100 ms 静音：不计
    y.extend(tone(SR / 2));
    y.extend(vec![0.0; PKT]); // 丢 1 包
    y.extend(tone(SR / 4));
    y.extend(vec![0.0; PKT]); // 丢 1 包
    y.extend(tone(SR / 4));
    y.extend(vec![0.0; 2 * PKT]); // 丢 2 包
    y.extend(tone(SR / 4));
    y.extend(vec![0.0; SR / 2]); // 500 ms：有意停顿，不计
    y.extend(tone(SR / 4));
    y.extend(vec![0.0; 10]); // 1.25 ms：短于一包，不计

    let mut docs = Docs::new();
    let id = open(&mut docs, &y);
    let r = loss(&docs, &id);

    assert_eq!(r["estimated"], true);
    assert_eq!(r["packetMs"], 20);
    assert_eq!(r["lostPackets"], 4);
    assert_eq!(r["gapCount"], 3);
    assert!((r["longestGapMs"].as_f64().unwrap() - 40.0).abs() < 1e-9);
    // 总包数 = 首个到最后一个语音帧（含停顿）：0.5+0.25*4 s 语音 + 80 ms 缺口 + 500 ms 停顿
    //        = 2.08 s = 104 包
    assert_eq!(r["totalPackets"], 104);
    assert!((r["lossRate"].as_f64().unwrap() - 4.0 / 104.0).abs() < 1e-12);
    // 第一个缺口紧跟 100 ms 前导 + 500 ms 语音
    assert!((r["gaps"][0]["startSec"].as_f64().unwrap() - 0.6).abs() < 1e-9);
}

#[test]
fn clean_and_silent_audio_have_no_loss() {
    let mut docs = Docs::new();
    let clean = open(&mut docs, &tone(SR));
    let r = loss(&docs, &clean);
    assert_eq!(r["lostPackets"], 0);
    assert_eq!(r["totalPackets"], 50);
    assert_eq!(r["lossRate"], 0.0);

    let silent = open(&mut docs, &vec![0.0; SR]);
    let r = loss(&docs, &silent);
    assert_eq!(r["totalPackets"], 0);
    assert_eq!(r["lossRate"], 0.0);
}

/// 立体声：左声道有缺口、右声道干净；按声道 + 区间统计
#[test]
fn per_channel_and_range() {
    let mut l = tone(SR / 2);
    l.extend(vec![0.0; PKT]); // 左声道 0.5 s 处丢 1 包
    l.extend(tone(SR / 2));
    let r = tone(l.len());
    let inter: Vec<f32> = l.iter().zip(&r).flat_map(|(a, b)| [*a, *b]).collect();
    let mut bytes = Vec::new();
    for v in &inter {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let mut docs = Docs::new();
    let id = dispatch_write(
        &mut docs,
        "doc_open_data",
        &json!({ "name": "st", "sampleRate": SR, "channels": 2,
                 "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes) }),
    )
    .unwrap()["docId"]
        .as_str()
        .unwrap()
        .to_string();
    let q = |extra: Value| {
        let mut p = json!({ "docId": id });
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        dispatch_read(&docs, "doc_packet_loss", &p).unwrap()
    };
    // 混合：右声道不为 0 → 帧不算静音 → 无丢包
    assert_eq!(q(json!({}))["lostPackets"], 0);
    assert_eq!(q(json!({ "channel": 0 }))["lostPackets"], 1);
    assert_eq!(q(json!({ "channel": 1 }))["lostPackets"], 0);
    // 只看后半段（缺口之后）：左声道也无丢包
    let after = q(json!({ "channel": 0, "begin": SR / 2 + PKT }));
    assert_eq!(after["lostPackets"], 0);
    assert_eq!(after["totalPackets"], 25);
    assert!(dispatch_read(&docs, "doc_packet_loss", &json!({ "docId": id, "channel": 2 })).is_err());
}

/// 有损编码后的缺口不是精确 0：用 −55 dBFS 的低噪声 + 5 ms 斜坡模拟，仍要检出；
/// 慢慢淡出到停顿（自然语音）不算丢包。
#[test]
fn codec_like_gap_counts_but_slow_fade_does_not() {
    let mut s = 0x1234_5678u32;
    let mut noise = move |amp: f32| {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        (s as f32 / u32::MAX as f32 - 0.5) * 2.0 * amp
    };
    let ramp = SR / 200; // 5 ms
    let mut y = tone(SR / 2);
    // 40 ms 缺口：底噪 ~ −55 dBFS，两侧 5 ms 线性过渡
    let n = 2 * PKT;
    for i in 0..n {
        let edge = (i.min(n - 1 - i) as f32 / ramp as f32).min(1.0);
        let speech = 0.3 + 0.2 * (i as f32 * 0.35).sin();
        y.push(speech * (1.0 - edge) + noise(0.003) * edge);
    }
    y.extend(tone(SR / 2));
    // 200 ms 自然停顿：100 ms 慢慢淡出到底噪，再 100 ms 底噪
    let fade = SR / 10;
    for i in 0..fade {
        let g = 1.0 - i as f32 / fade as f32;
        y.push((0.3 + 0.2 * (i as f32 * 0.35).sin()) * g * g + noise(0.003));
    }
    for _ in 0..SR / 10 {
        y.push(noise(0.003));
    }
    y.extend(tone(SR / 2));

    let mut docs = Docs::new();
    let id = open(&mut docs, &y);
    let r = loss(&docs, &id);
    assert_eq!(r["gapCount"], 1, "{r}");
    assert_eq!(r["lostPackets"], 2);
    let g = &r["gaps"][0];
    assert!((g["startSec"].as_f64().unwrap() - 0.5).abs() < 0.011, "{g}");
    assert!(g["depthDb"].as_f64().unwrap() >= 25.0);
}
