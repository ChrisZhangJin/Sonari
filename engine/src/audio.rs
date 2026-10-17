//! 音频设备层 —— cpal 播放 / 录音（对标 ocenaudio 的 libqtocenmixer2 + WASAPI 层）。
//!
//! 命令（cmd 以 `audio_` 前缀路由到本模块）：
//! - `audio_devices`      枚举输出/输入设备
//! - `playback_start`     从指定帧区间开始播放（可选设备名，线性插值重采样）
//! - `playback_status`    轮询播放位置
//! - `playback_stop`      停止播放
//! - `record_start`       开始录音（可选输入设备名）
//! - `record_status`      轮询录音时长
//! - `record_stop`        停止并取回 f32 交错样本
//!
//! 同一时刻最多一个播放流 + 一个录音流；流句柄 drop 即停止。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::Engine as _;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SizedSample, Stream, StreamConfig};
use serde_json::{json, Value};

use crate::commands::Docs;
use crate::decode::AudioBuffer;

/// 全局音频状态（两个宿主各持一份 Arc）。
#[derive(Default)]
pub struct AudioState {
    playback: Mutex<Option<PlaybackHandle>>,
    record: Mutex<Option<RecordHandle>>,
}

struct PlaybackHandle {
    _stream: Stream,
    pos_out_frames: Arc<AtomicU64>,
    dev_sr: u32,
    src_sr: u32,
    /// 播放起点（源帧）——状态里的 positionSec 是**绝对**文档时间
    begin: usize,
    finished: Arc<AtomicBool>,
}

struct RecordHandle {
    _stream: Stream,
    buf: Arc<Mutex<Vec<f32>>>,
    sr: u32,
    ch: u16,
    started_at: Instant,
}

pub fn dispatch(st: &AudioState, docs: &Docs, cmd: &str, p: &Value) -> Result<Value, String> {
    match cmd {
        "audio_devices" => audio_devices(),
        "playback_start" => playback_start(st, docs, p),
        "playback_status" => playback_status(st),
        "playback_stop" => playback_stop(st),
        "record_start" => record_start(st, p),
        "record_status" => record_status(st),
        "record_stop" => record_stop(st),
        other => Err(format!("unknown audio cmd: {other}")),
    }
}

// ---------- 设备枚举 ----------

fn devices_of(host: &cpal::Host, input: bool) -> Result<Vec<Value>, String> {
    let default = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    };
    let default_name = default
        .as_ref()
        .and_then(|d| d.description().ok())
        .map(|x| x.name().to_string());
    let devs = host.devices().map_err(|e| format!("devices: {e}"))?;
    let mut out = Vec::new();
    for d in devs {
        let name = match d.description() {
            Ok(desc) => desc.name().to_string(),
            Err(_) => continue,
        };
        // 只列真正能开的设备：默认流配置取不到的一律跳过
        // （否则会把麦克风列进输出列表，选中后播放必然失败）
        let cfg = match if input {
            d.default_input_config()
        } else {
            d.default_output_config()
        } {
            Ok(c) => c,
            Err(_) => continue,
        };
        let meta = format!(
            "{} kHz · {} 声道",
            cfg.sample_rate() as f64 / 1000.0,
            cfg.channels()
        );
        out.push(json!({
            "name": name,
            "meta": meta,
            "isDefault": default_name.as_deref() == Some(name.as_str()),
        }));
    }
    Ok(out)
}

fn audio_devices() -> Result<Value, String> {
    let host = cpal::default_host();
    Ok(json!({
        "host": format!("{:?}", host.id()),
        "outputs": devices_of(&host, false)?,
        "inputs": devices_of(&host, true)?,
    }))
}

fn find_device(host: &cpal::Host, name: Option<&str>, input: bool) -> Result<cpal::Device, String> {
    let devs = host.devices().map_err(|e| format!("devices: {e}"))?;
    if let Some(want) = name {
        for d in devs {
            if let Ok(desc) = d.description() {
                if desc.name() == want {
                    return Ok(d);
                }
            }
        }
        return Err(format!("未找到设备: {want}"));
    }
    let default = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    };
    default.ok_or_else(|| "没有可用的默认设备".into())
}

// ---------- 播放 ----------

fn playback_start(st: &AudioState, docs: &Docs, p: &Value) -> Result<Value, String> {
    let doc = docs.get(p.get("docId").and_then(|x| x.as_str()).unwrap_or(""))?;
    let buf: Arc<AudioBuffer> = doc.buf.clone();
    let src_ch = buf.channels.max(1);
    let src_sr = buf.sample_rate as f64;
    let total_frames = buf.frames();
    let begin = (p.get("begin").and_then(|x| x.as_u64()).unwrap_or(0) as usize).min(total_frames);
    let end = (p.get("end").and_then(|x| x.as_u64()).unwrap_or(total_frames as u64) as usize)
        .min(total_frames)
        .max(begin);
    if end == begin {
        return Err("播放区间为空".into());
    }

    // 若已有播放流，先停掉
    *st.playback.lock().unwrap() = None;

    let host = cpal::default_host();
    let want = p
        .get("device")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty());
    let device = find_device(&host, want, false)?;
    let dev_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_default();
    let cfg = device
        .default_output_config()
        .map_err(|e| format!("输出配置: {e}"))?;
    let dev_ch = cfg.channels() as usize;
    let dev_sr = cfg.sample_rate();

    let pos = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let ratio = src_sr / dev_sr as f64;
    let span = end - begin;

    // 采样格式回退：优先 f32（WASAPI 主流），失败再试 i16 / u16
    let (stream, fmt_name) = build_out::<f32>(
        &device,
        dev_ch,
        dev_sr,
        buf.clone(),
        src_ch,
        begin,
        span,
        ratio,
        pos.clone(),
        finished.clone(),
        |v| v,
    )
    .map(|s| (s, "f32"))
    .or_else(|_| {
        build_out::<i16>(
            &device,
            dev_ch,
            dev_sr,
            buf.clone(),
            src_ch,
            begin,
            span,
            ratio,
            pos.clone(),
            finished.clone(),
            |v| (v * 32767.0).clamp(-32768.0, 32767.0) as i16,
        )
        .map(|s| (s, "i16"))
    })
    .or_else(|_| {
        build_out::<u16>(
            &device,
            dev_ch,
            dev_sr,
            buf.clone(),
            src_ch,
            begin,
            span,
            ratio,
            pos.clone(),
            finished.clone(),
            |v| ((v * 32767.0) + 32768.0).clamp(0.0, 65535.0) as u16,
        )
        .map(|s| (s, "u16"))
    })
    .map_err(|e| format!("创建播放流: {e}"))?;
    stream.play().map_err(|e| format!("启动播放: {e}"))?;

    *st.playback.lock().unwrap() = Some(PlaybackHandle {
        _stream: stream,
        pos_out_frames: pos,
        dev_sr,
        src_sr: buf.sample_rate,
        begin,
        finished,
    });

    Ok(json!({
        "device": dev_name,
        "devSampleRate": dev_sr,
        "devChannels": dev_ch,
        "format": fmt_name,
        "begin": begin,
        "end": end,
        "startSec": begin as f64 / buf.sample_rate as f64,
    }))
}

/// 输出流：线性插值重采样 + 逐样本格式转换。游标在闭包外捕获（跨回调保持）。
#[allow(clippy::too_many_arguments)]
fn build_out<T: SizedSample + 'static>(
    device: &cpal::Device,
    dev_ch: usize,
    dev_sr: u32,
    src: Arc<AudioBuffer>,
    src_ch: usize,
    begin: usize,
    span: usize,
    ratio: f64,
    pos_cb: Arc<AtomicU64>,
    fin_cb: Arc<AtomicBool>,
    cvt: fn(f32) -> T,
) -> Result<Stream, String> {
    let mut cursor = 0f64; // 区间内源帧位置（f64）——闭包间保持
    device
        .build_output_stream(
            StreamConfig {
                channels: dev_ch as u16,
                sample_rate: dev_sr,
                buffer_size: cpal::BufferSize::Default,
            },
            move |out: &mut [T], _| {
                let rel_out = out.len() / dev_ch.max(1);
                for k in 0..rel_out {
                    if cursor >= span as f64 {
                        fin_cb.store(true, Ordering::Relaxed);
                        break; // 其余保持静默（回调 slice 预填充静音）
                    }
                    let i0 = cursor.floor();
                    let frac = (cursor - i0) as f32;
                    let f0 = begin + (i0 as usize).min(span);
                    let f1 = begin + (i0 as usize + 1).min(span);
                    for c in 0..dev_ch {
                        let sc = c.min(src_ch - 1);
                        let a = sample_at(&src, f0, sc, src_ch);
                        let b = sample_at(&src, f1, sc, src_ch);
                        out[k * dev_ch + c] = cvt(a + (b - a) * frac);
                    }
                    cursor += ratio;
                }
                pos_cb.fetch_add(rel_out as u64, Ordering::Relaxed);
                if cursor >= span as f64 {
                    fin_cb.store(true, Ordering::Relaxed);
                }
            },
            |e| eprintln!("playback stream: {e}"),
            None,
        )
        .map_err(|e| format!("{e}"))
}

#[inline]
fn sample_at(buf: &AudioBuffer, frame: usize, ch: usize, nch: usize) -> f32 {
    buf.samples.get(frame * nch + ch).copied().unwrap_or(0.0)
}

fn playback_status(st: &AudioState) -> Result<Value, String> {
    let guard = st.playback.lock().unwrap();
    match &*guard {
        Some(h) => {
            let pos = h.pos_out_frames.load(Ordering::Relaxed);
            let finished = h.finished.load(Ordering::Relaxed);
            // 绝对文档时间 = 起点秒 + 已播放秒（设备时间与源时间同为真实时间）
            let position_sec = h.begin as f64 / h.src_sr as f64 + pos as f64 / h.dev_sr as f64;
            Ok(json!({
                "playing": !finished,
                "positionSec": position_sec,
            }))
        }
        None => Ok(json!({ "playing": false, "positionSec": 0.0 })),
    }
}

fn playback_stop(st: &AudioState) -> Result<Value, String> {
    drop(st.playback.lock().unwrap().take());
    Ok(Value::Null)
}

// ---------- 录音 ----------

fn record_start(st: &AudioState, p: &Value) -> Result<Value, String> {
    // 若已在录音，先停
    *st.record.lock().unwrap() = None;

    let host = cpal::default_host();
    let want = p
        .get("device")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty());
    let device = find_device(&host, want, true)?;
    let dev_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_default();
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("输入配置: {e}"))?;
    let sr = cfg.sample_rate();
    let ch = cfg.channels();
    let fmt = cfg.sample_format();

    let buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let buf_cb = buf.clone();

    let stream = match fmt {
        cpal::SampleFormat::F32 => build_input::<f32>(&device, &cfg, move |s| {
            buf_cb.lock().unwrap().extend_from_slice(s)
        })?,
        cpal::SampleFormat::I16 => build_input::<i16>(&device, &cfg, move |s| {
            buf_cb
                .lock()
                .unwrap()
                .extend(s.iter().map(|v| *v as f32 / 32768.0))
        })?,
        cpal::SampleFormat::U16 => build_input::<u16>(&device, &cfg, move |s| {
            buf_cb
                .lock()
                .unwrap()
                .extend(s.iter().map(|v| (*v as f32 - 32768.0) / 32768.0))
        })?,
        other => return Err(format!("不支持的输入采样格式: {other:?}")),
    };
    stream.play().map_err(|e| format!("启动录音: {e}"))?;

    *st.record.lock().unwrap() = Some(RecordHandle {
        _stream: stream,
        buf,
        sr,
        ch,
        started_at: Instant::now(),
    });
    Ok(json!({
        "device": dev_name,
        "sampleRate": sr,
        "channels": ch,
        "format": format!("{fmt:?}")
    }))
}

fn build_input<T: SizedSample>(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    mut sink: impl FnMut(&[T]) + Send + 'static,
) -> Result<Stream, String> {
    device
        .build_input_stream(
            StreamConfig {
                channels: cfg.channels(),
                sample_rate: cfg.sample_rate(),
                buffer_size: cpal::BufferSize::Default,
            },
            move |data: &[T], _| sink(data),
            |e| eprintln!("record stream: {e}"),
            None,
        )
        .map_err(|e| format!("创建录音流: {e}"))
}

fn record_status(st: &AudioState) -> Result<Value, String> {
    let guard = st.record.lock().unwrap();
    match &*guard {
        Some(h) => Ok(json!({
            "recording": true,
            "seconds": h.started_at.elapsed().as_secs_f64(),
            "sampleRate": h.sr,
            "channels": h.ch,
        })),
        None => Ok(json!({ "recording": false, "seconds": 0.0 })),
    }
}

fn record_stop(st: &AudioState) -> Result<Value, String> {
    let handle = st.record.lock().unwrap().take();
    let Some(h) = handle else {
        return Err("没有正在进行的录音".into());
    };
    drop(h._stream); // 停流
    let samples = h.buf.lock().unwrap().clone();
    let frames = samples.len() / h.ch.max(1) as usize;
    if frames == 0 {
        return Err("录音为空".into());
    }
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for v in &samples {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Ok(json!({
        "sampleRate": h.sr,
        "channels": h.ch,
        "frames": frames,
        "durationSec": h.started_at.elapsed().as_secs_f64(),
        "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}
