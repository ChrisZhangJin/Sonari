//! 「查看丢包率」：没有真实包信息时按数字静音缺口估算。

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
    // 有声区间 = 0.5+0.25*4 s 语音 + 80 ms 缺口 = 1.58 s = 79 包
    assert_eq!(r["totalPackets"], 79);
    assert!((r["lossRate"].as_f64().unwrap() - 4.0 / 79.0).abs() < 1e-12);
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
