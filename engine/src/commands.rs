//! 命令层 —— WS 服务端与 Tauri 桌面端共用的命令实现。
//!
//! 参数/返回均为 `serde_json::Value`（camelCase 键，与前端契约一致）。
//! 全部同步函数：调用方（WS 的 spawn_blocking / Tauri 的 async_runtime）
//! 负责放到阻塞线程池。`spectrogram/constellation/pcm` 只读（RwLock 读锁
//! 并发），`doc_open/doc_close` 写。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::analysis::{constellation, level_stats, spectrogram, ConstellationConfig, SpectralConfig};
use crate::decode::{decode_file_with_meta, AudioBuffer, StreamMeta};
use crate::dsp::WindowType;
use crate::encode::EncodeOptions;

const PEAK_BUCKETS: usize = 4096;

/// 已打开的文档：样本 + 打开时算好的波形峰值缓存（对应 ocenaudio 的
/// AudioForm 后台线程产物：peaks 只算一次，缩放时重采样）。
pub struct DocEntry {
    pub name: String,
    /// 来源路径（录音产物为空）
    pub path: String,
    pub buf: Arc<AudioBuffer>,
    /// 每 bucket [min, max]（mono 混合）
    pub peaks: Vec<[f32; 2]>,
    /// 码流元数据（分辨率 / 编解码器 ——「查看信息」用）
    pub meta: StreamMeta,
    /// 操作历史（快照式）：undo 栈顶 = 最近一次操作之前的音频
    pub undo: Vec<HistItem>,
    /// 被撤销的操作：栈顶 = 下一个可重做的
    pub redo: Vec<HistItem>,
}

/// 一步历史：操作名 + 该操作「另一侧」的音频快照。
/// 缓冲区是 `Arc` 不可变共享，每次编辑本来就生成新缓冲区，存快照不额外复制。
pub struct HistItem {
    pub label: String,
    pub buf: Arc<AudioBuffer>,
}

/// 历史上限：步数 + 快照总样本数（约 1 GiB f32），超出丢最老的
const HISTORY_MAX_STEPS: usize = 100;
const HISTORY_MAX_SAMPLES: usize = 256 * 1024 * 1024;

#[derive(Default)]
pub struct Docs {
    inner: HashMap<String, DocEntry>,
    /// 剪贴板（剪切 / 复制的音频），跨文档共享
    clipboard: Option<Arc<AudioBuffer>>,
}

impl Docs {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, id: &str) -> Result<&DocEntry, String> {
        self.inner
            .get(id)
            .ok_or_else(|| format!("doc not found: {id}"))
    }
    pub fn get_mut(&mut self, id: &str) -> Result<&mut DocEntry, String> {
        self.inner
            .get_mut(id)
            .ok_or_else(|| format!("doc not found: {id}"))
    }
}

fn doc_id_of(p: &Value) -> String {
    p.get("docId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// 峰值缓存 + 响度统计 —— 打开与「转换」后刷新共用。
fn peaks_and_stats(buf: &AudioBuffer) -> (Vec<[f32; 2]>, Option<Value>) {
    let mono = buf.to_mono();
    let mut peaks = vec![[0.0f32; 2]; PEAK_BUCKETS];
    if !mono.is_empty() {
        let per = mono.len() / PEAK_BUCKETS;
        if per > 0 {
            for (i, pk) in peaks.iter_mut().enumerate() {
                let seg = &mono[i * per..(i + 1) * per];
                let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
                for v in seg {
                    mn = mn.min(*v);
                    mx = mx.max(*v);
                }
                pk[0] = mn;
                pk[1] = mx;
            }
        }
    }
    let stats = level_stats(&mono, buf.sample_rate, 1).map(|s| {
        json!({
            "integratedLufs": s.integrated_lufs,
            "loudnessRangeLu": s.loudness_range_lu,
            "truePeakDbtp": s.true_peak_dbtp,
            "samplePeakDbfs": s.sample_peak_dbfs,
        })
    });
    (peaks, stats)
}

/// 文档 → 前端契约 JSON（与 `doc_open` 同构，`doc_resample`/`doc_channels` 复用）。
fn entry_json(doc_id: &str, e: &DocEntry, stats: Option<Value>) -> Value {
    json!({
        "docId": doc_id,
        "name": e.name,
        // 前端「保存 / 导出 / 另存为 / 重命名」都靠它判断文档是否已关联磁盘文件
        "path": e.path,
        "info": {
            "sampleRate": e.buf.sample_rate,
            "channels": e.buf.channels,
            "channelsText": channels_text(e.buf.channels),
            "frames": e.buf.frames(),
            "durationSec": e.buf.duration_secs(),
            "bitsPerSample": e.meta.bits_per_sample,
            "codec": e.meta.codec,
            "codecLabel": e.meta.codec_label(),
        },
        "peaks": e.peaks.iter().flat_map(|pk| pk.iter().copied()).collect::<Vec<f32>>(),
        "stats": stats,
        "history": history_json(e),
    })
}

/// 历史列表：undo 从旧到新，redo 从「下一个可重做」开始
fn history_json(e: &DocEntry) -> Value {
    json!({
        "undo": e.undo.iter().map(|h| h.label.as_str()).collect::<Vec<_>>(),
        "redo": e.redo.iter().rev().map(|h| h.label.as_str()).collect::<Vec<_>>(),
    })
}

/// 重算峰值/响度并出 JSON（任何改了 `buf` 的命令收尾用）
fn refresh(id: &str, e: &mut DocEntry) -> Value {
    let (peaks, stats) = peaks_and_stats(&e.buf);
    e.peaks = peaks;
    entry_json(id, e, stats)
}

/// 提交一次编辑：旧音频入 undo 栈、清空 redo、换上新音频
fn commit(id: &str, e: &mut DocEntry, label: impl Into<String>, new_buf: AudioBuffer) -> Value {
    let old = std::mem::replace(&mut e.buf, Arc::new(new_buf));
    e.undo.push(HistItem { label: label.into(), buf: old });
    e.redo.clear();
    let mut total: usize = e.undo.iter().map(|h| h.buf.samples.len()).sum();
    while e.undo.len() > 1 && (e.undo.len() > HISTORY_MAX_STEPS || total > HISTORY_MAX_SAMPLES) {
        total -= e.undo.remove(0).buf.samples.len();
    }
    refresh(id, e)
}

/// 区间参数 begin/end（帧），钳到文档内且非空
fn span_of(p: &Value, frames: usize) -> Result<(usize, usize), String> {
    let end = (as_u64(p, "end", frames as u64) as usize).min(frames);
    let begin = (as_u64(p, "begin", 0) as usize).min(end);
    if begin >= end {
        return Err("选区为空".into());
    }
    Ok((begin, end))
}

fn fmt_secs(frames: usize, rate: u32) -> String {
    format!("{:.3}s", frames as f64 / rate.max(1) as f64)
}

fn clip_json(c: &Option<Arc<AudioBuffer>>) -> Value {
    match c {
        Some(b) => json!({
            "frames": b.frames(),
            "sampleRate": b.sample_rate,
            "channels": b.channels,
            "durationSec": b.duration_secs(),
        }),
        None => Value::Null,
    }
}

/// 剪切：区间进剪贴板，并从文档删掉
fn doc_cut(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let e = docs.get_mut(&id)?;
    let (b, end) = span_of(p, e.buf.frames())?;
    if b == 0 && end == e.buf.frames() {
        return Err("不能剪掉全部音频（可用「静音」）".into());
    }
    let ch = e.buf.channels.max(1);
    let (sr, chn) = (e.buf.sample_rate, e.buf.channels);
    let clip = e.buf.samples[b * ch..end * ch].to_vec();
    let mut rest = Vec::with_capacity(e.buf.samples.len() - clip.len());
    rest.extend_from_slice(&e.buf.samples[..b * ch]);
    rest.extend_from_slice(&e.buf.samples[end * ch..]);
    let label = format!("剪切 {}", fmt_secs(end - b, sr));
    let resp = commit(&id, e, label, AudioBuffer { sample_rate: sr, channels: chn, samples: rest });
    docs.clipboard = Some(Arc::new(AudioBuffer { sample_rate: sr, channels: chn, samples: clip }));
    Ok(resp)
}

/// 复制：区间进剪贴板（文档不变）
fn doc_copy(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let e = docs.get(&id)?;
    let (b, end) = span_of(p, e.buf.frames())?;
    let ch = e.buf.channels.max(1);
    let clip = AudioBuffer {
        sample_rate: e.buf.sample_rate,
        channels: e.buf.channels,
        samples: e.buf.samples[b * ch..end * ch].to_vec(),
    };
    docs.clipboard = Some(Arc::new(clip));
    Ok(clip_json(&docs.clipboard))
}

/// 粘贴剪贴板：有 begin<end 则替换该区间，否则插入到 frame。
/// 采样率 / 声道与目标文档不一致时自动转换。
fn doc_paste_clip(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let clip = docs.clipboard.clone().ok_or_else(|| "剪贴板为空".to_string())?;
    let e = docs.get_mut(&id)?;
    let frames = e.buf.frames();
    let ch = e.buf.channels.max(1);
    let mut clip: AudioBuffer = (*clip).clone();
    if clip.channels != e.buf.channels {
        clip = crate::transform::convert_channels(&clip, e.buf.channels);
    }
    if clip.sample_rate != e.buf.sample_rate {
        clip = crate::transform::resample(&clip, e.buf.sample_rate);
    }
    let (b, end) = match span_of(p, frames) {
        Ok(r) if p.get("begin").is_some() => r,
        _ => {
            let at = (as_u64(p, "frame", 0) as usize).min(frames);
            (at, at)
        }
    };
    // 容量按「原长 - 替换区 + 剪贴板长」估算；用 saturating_sub 防止下溢
    let replaced = (end - b).saturating_mul(ch);
    let cap = e.buf.samples.len().saturating_sub(replaced) + clip.samples.len();
    let mut out = Vec::with_capacity(cap);
    out.extend_from_slice(&e.buf.samples[..b * ch]);
    out.extend_from_slice(&clip.samples);
    out.extend_from_slice(&e.buf.samples[end * ch..]);
    let label = if end > b {
        format!("粘贴（替换 {}）", fmt_secs(end - b, e.buf.sample_rate))
    } else {
        format!("粘贴 {}", fmt_secs(clip.frames(), e.buf.sample_rate))
    };
    let (sr, chn) = (e.buf.sample_rate, e.buf.channels);
    Ok(commit(&id, e, label, AudioBuffer { sample_rate: sr, channels: chn, samples: out }))
}

/// 静音：区间置 0
fn doc_silence(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let e = docs.get_mut(&id)?;
    let (b, end) = span_of(p, e.buf.frames())?;
    let ch = e.buf.channels.max(1);
    let mut out = (*e.buf).clone();
    out.samples[b * ch..end * ch].fill(0.0);
    let label = format!("静音 {}", fmt_secs(end - b, out.sample_rate));
    Ok(commit(&id, e, label, out))
}

/// 淡入 / 淡出：区间内的增益包络。
/// 给 `curve`（≥2 个 0..=1 的增益，沿选区等距采样）就按它线性插值；
/// 否则线性 0→1（dir="in"）或 1→0（dir="out"）。
fn doc_fade(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let fade_in = match p.get("dir").and_then(|x| x.as_str()) {
        Some("in") => true,
        Some("out") => false,
        _ => return Err("dir 需为 in 或 out".into()),
    };
    let curve: Option<Vec<f32>> = match p.get("curve") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| "curve 需为数组".to_string())?;
            let c: Vec<f32> = arr
                .iter()
                .map(|x| x.as_f64().map(|g| g.clamp(0.0, 1.0) as f32))
                .collect::<Option<_>>()
                .ok_or_else(|| "curve 需为数值数组".to_string())?;
            if c.len() < 2 {
                return Err("curve 至少需要 2 个点".into());
            }
            Some(c)
        }
    };
    let e = docs.get_mut(&id)?;
    let (b, end) = span_of(p, e.buf.frames())?;
    let ch = e.buf.channels.max(1);
    let mut out = (*e.buf).clone();
    let n = end - b;
    let denom = (n.max(2) - 1) as f32;
    let gain_at = |x: f32| -> f32 {
        match &curve {
            Some(c) => {
                let pos = x * (c.len() - 1) as f32;
                let i = (pos.floor() as usize).min(c.len() - 2);
                let fr = pos - i as f32;
                c[i] + (c[i + 1] - c[i]) * fr
            }
            None if fade_in => x,
            None => 1.0 - x,
        }
    };
    for k in 0..n {
        let x = k as f32 / denom; // 0..=1
        let g = gain_at(x);
        for v in &mut out.samples[(b + k) * ch..(b + k + 1) * ch] {
            *v *= g;
        }
    }
    let label = format!(
        "{} {}{}",
        if fade_in { "淡入" } else { "淡出" },
        fmt_secs(n, out.sample_rate),
        if curve.is_some() { "（曲线）" } else { "" }
    );
    Ok(commit(&id, e, label, out))
}

/// 撤销 steps 步（默认 1）
fn doc_undo(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let e = docs.get_mut(&id)?;
    let steps = as_u64(p, "steps", 1).max(1) as usize;
    if e.undo.is_empty() {
        return Err("没有可撤销的操作".into());
    }
    for _ in 0..steps {
        let Some(h) = e.undo.pop() else { break };
        let cur = std::mem::replace(&mut e.buf, h.buf);
        e.redo.push(HistItem { label: h.label, buf: cur });
    }
    Ok(refresh(&id, e))
}

/// 重做 steps 步（默认 1）
fn doc_redo(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let e = docs.get_mut(&id)?;
    let steps = as_u64(p, "steps", 1).max(1) as usize;
    if e.redo.is_empty() {
        return Err("没有可重做的操作".into());
    }
    for _ in 0..steps {
        let Some(h) = e.redo.pop() else { break };
        let cur = std::mem::replace(&mut e.buf, h.buf);
        e.undo.push(HistItem { label: h.label, buf: cur });
    }
    Ok(refresh(&id, e))
}

fn as_u64(v: &Value, key: &str, default: u64) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(default)
}

fn as_f64(v: &Value, key: &str, default: f64) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(default)
}

fn next_id() -> String {
    static N: AtomicU64 = AtomicU64::new(1);
    format!("d{}", N.fetch_add(1, Ordering::Relaxed))
}

/// 写命令路由（改文档）。
pub fn dispatch_write(docs: &mut Docs, cmd: &str, p: &Value) -> Result<Value, String> {
    match cmd {
        "doc_open" => doc_open(docs, p),
        "doc_open_data" => doc_open_data(docs, p),
        "doc_close" => doc_close(docs, p),
        "doc_resample" => doc_resample(docs, p),
        "doc_channels" => doc_channels(docs, p),
        "doc_rename" => doc_rename(docs, p),
        "doc_save" => doc_save(docs, p),
        "doc_paste" => doc_paste(docs, p),
        "doc_cut" => doc_cut(docs, p),
        "doc_copy" => doc_copy(docs, p),
        "doc_paste_clip" => doc_paste_clip(docs, p),
        "doc_silence" => doc_silence(docs, p),
        "doc_fade" => doc_fade(docs, p),
        "doc_extract_channel" => doc_extract_channel(docs, p),
        "doc_undo" => doc_undo(docs, p),
        "doc_redo" => doc_redo(docs, p),
        other => Err(format!("unknown cmd: {other}")),
    }
}

/// 读命令路由（只读分析）。
pub fn dispatch_read(docs: &Docs, cmd: &str, p: &Value) -> Result<Value, String> {
    match cmd {
        "waveform" => waveform_cmd(docs, p),
        "spectrogram" => spectrogram_cmd(docs, p),
        "constellation" => constellation_cmd(docs, p),
        "doc_info" => doc_info(docs, p),
        "encoder_caps" => Ok(crate::encode::capabilities()),
        "pcm" => pcm(docs, p),
        "clipboard_info" => Ok(clip_json(&docs.clipboard)),
        "mss_constellation" => mss_constellation(docs, p),
        "doc_packet_loss" => doc_packet_loss(docs, p),
        other => Err(format!("unknown cmd: {other}")),
    }
}

/// 公共部分：峰值缓存 + 响度统计 + 响应 JSON + 入库。
fn build_doc_entry(
    name: String,
    path: String,
    buf: Arc<AudioBuffer>,
    meta: StreamMeta,
) -> (Value, DocEntry) {
    let (peaks, stats) = peaks_and_stats(&buf);
    let doc_id = next_id();
    let entry = DocEntry {
        name,
        path,
        buf,
        peaks,
        meta,
        undo: Vec::new(),
        redo: Vec::new(),
    };
    let resp = entry_json(&doc_id, &entry, stats);
    (resp, entry)
}

/// 「单声道 / 双声道 / N 声道」——「查看信息」与 LCD 共用。
pub fn channels_text(ch: usize) -> String {
    match ch {
        1 => "单声道 (Mono)".into(),
        2 => "双声道 (Stereo)".into(),
        n => format!("{n} 声道"),
    }
}

fn doc_open(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let path = p
        .get("path")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "path required".to_string())?;
    let (buf, meta) =
        decode_file_with_meta(Path::new(path)).map_err(|e| format!("decode: {e}"))?;
    let buf = Arc::new(buf);
    let name = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let (resp, entry) = build_doc_entry(name, path.to_string(), buf, meta);
    docs.inner.insert(resp["docId"].as_str().unwrap().to_string(), entry);
    Ok(resp)
}

/// 「查看信息」——对标 ocenaudio 的「信号属性」对话框：
/// 文件名 / 路径 / 容器 / 编解码器 / 分辨率 / 通道 / 采样率 / 时长 / 
/// 样本数 / 文件大小 / 平均比特率 / 响度统计。
fn doc_info(docs: &Docs, p: &Value) -> Result<Value, String> {
    let doc = docs.get(p.get("docId").and_then(|x| x.as_str()).unwrap_or(""))?;
    let buf = &doc.buf;
    let ch = buf.channels.max(1);
    let frames = buf.frames();
    let dur = buf.duration_secs();

    let file_bytes = if doc.path.is_empty() {
        None
    } else {
        std::fs::metadata(&doc.path).ok().map(|m| m.len())
    };
    // 容器名取扩展名（symphonia 的 FormatReader 不暴露容器名）
    let container = doc
        .path
        .rsplit('.')
        .next()
        .filter(|s| !s.is_empty() && doc.path.contains('.'))
        .map(|s| s.to_ascii_uppercase());
    let bitrate_kbps = match (file_bytes, dur > 0.0) {
        (Some(b), true) => Some(b as f64 * 8.0 / dur / 1000.0),
        _ => None,
    };
    // 逐声道样本峰值（dBFS）—— 与整体 LUFS 互补，看单声道是否过载
    let channel_peaks_dbfs: Vec<f64> = (0..ch)
        .map(|c| {
            let mut mx = 0.0f32;
            let mut i = c;
            while i < buf.samples.len() {
                let v = buf.samples[i].abs();
                if v > mx {
                    mx = v;
                }
                i += ch;
            }
            if mx > 0.0 {
                20.0 * (mx as f64).log10()
            } else {
                -f64::INFINITY
            }
        })
        .collect();

    let mono = buf.to_mono();
    let stats = level_stats(&mono, buf.sample_rate, 1).map(|s| {
        json!({
            "integratedLufs": s.integrated_lufs,
            "loudnessRangeLu": s.loudness_range_lu,
            "truePeakDbtp": s.true_peak_dbtp,
            "samplePeakDbfs": s.sample_peak_dbfs,
        })
    });

    Ok(json!({
        "docId": p.get("docId").and_then(|x| x.as_str()).unwrap_or(""),
        "name": doc.name,
        "path": doc.path,
        "container": container,
        "codec": doc.meta.codec,
        "codecLabel": doc.meta.codec_label(),
        "sampleRate": buf.sample_rate,
        "channels": ch,
        "channelsText": channels_text(ch),
        "frames": frames,
        "durationSec": dur,
        "bitsPerSample": doc.meta.bits_per_sample,
        "bitsPerCodedSample": doc.meta.bits_per_coded_sample,
        "fileBytes": file_bytes,
        "bitrateKbps": bitrate_kbps,
        "channelPeaksDbfs": channel_peaks_dbfs,
        "stats": stats,
    }))
}

/// 从内存打开（录音产物）：参数含 name/sampleRate/channels/f32leBase64（交错）。
fn doc_open_data(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    use base64::Engine as _;
    let name = p
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("未命名")
        .to_string();
    let sample_rate = p.get("sampleRate").and_then(|x| x.as_u64()).unwrap_or(44100) as u32;
    let channels = (p.get("channels").and_then(|x| x.as_u64()).unwrap_or(1) as usize).max(1);
    let b64 = p
        .get("f32leBase64")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "f32leBase64 required".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("base64: {e}"))?;
    if bytes.is_empty() {
        return Err("音频数据为空".into());
    }
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let buf = Arc::new(AudioBuffer {
        sample_rate,
        channels,
        samples,
    });
    let meta = StreamMeta::pcm_f32();
    let (resp, entry) = build_doc_entry(name, String::new(), buf, meta);
    docs.inner.insert(resp["docId"].as_str().unwrap().to_string(), entry);
    Ok(resp)
}

fn doc_close(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let doc_id = p.get("docId").and_then(|x| x.as_str()).unwrap_or("");
    docs.inner.remove(doc_id);
    Ok(Value::Null)
}

// ---------- 「转换为」菜单：声道 / 采样率 ----------

/// 转换采样率：**原地改文档**，帧数按比例缩放（时长不变）。
/// 采样率改变后原文件信息不再适用，返回的 info 反映转换后的真实状态。
fn doc_resample(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let rate = p
        .get("sampleRate")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "sampleRate required".to_string())? as u32;
    if !(4_000..=384_000).contains(&rate) {
        return Err(format!("不支持的采样率 {rate} Hz（允许 4000–384000）"));
    }

    let entry = docs.get_mut(&id)?;
    if entry.buf.sample_rate == rate {
        return Err(format!("当前已是 {rate} Hz，无需转换"));
    }
    let new_buf = crate::transform::resample(&entry.buf, rate);
    Ok(commit(&id, entry, format!("转换采样率 → {rate} Hz"), new_buf))
}

/// 转换声道布局：1 = 下混为单声道，2 = 上混为立体声。
fn doc_channels(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let target = p
        .get("channels")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "channels required".to_string())? as usize;
    if !(1..=8).contains(&target) {
        return Err(format!("不支持的声道数 {target}（允许 1–8）"));
    }

    let entry = docs.get_mut(&id)?;
    if entry.buf.channels == target {
        return Err(format!("当前已是{}", channels_text(target)));
    }
    let new_buf = crate::transform::convert_channels(&entry.buf, target);
    Ok(commit(&id, entry, format!("转换为{}", channels_text(target)), new_buf))
}

/// 「粘贴 PCM」—— 在文档指定帧位置插入 f32le base64 数据。
fn doc_paste(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let frame = p.get("frame").and_then(|x| x.as_u64()).unwrap_or(0);
    let b64 = p
        .get("f32leBase64")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "f32leBase64 required".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("base64: {e}"))?;
    if bytes.is_empty() {
        return Err("音频数据为空".into());
    }
    let new_samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    let entry = docs.get_mut(&id)?;
    let ch = entry.buf.channels.max(1);
    let at = (frame as usize * ch).min(entry.buf.samples.len());
    let mut samples = (*entry.buf.samples).to_vec();
    // 如果插入数据声道不匹配，做简单的单/双声道适配
    let adapted: Vec<f32> = if new_samples.len() % ch != 0 {
        // 假设单声道数据插入多声道文档：每帧复制到所有声道
        let src_frames = new_samples.len();
        let mut out = Vec::with_capacity(src_frames * ch);
        for s in &new_samples {
            for _ in 0..ch {
                out.push(*s);
            }
        }
        out
    } else {
        new_samples
    };

    samples.splice(at..at, adapted.iter().copied());
    let new_buf = AudioBuffer {
        sample_rate: entry.buf.sample_rate,
        channels: entry.buf.channels,
        samples,
    };
    let label = format!("插入录音 {}", fmt_secs(adapted.len() / ch, new_buf.sample_rate));
    Ok(commit(&id, entry, label, new_buf))
}

// ---------- 文件操作：重命名 / 保存 / 另存为 / 导出 ----------

/// 「重命名…」—— 有磁盘文件时改文件名（同目录），录音等内存文档只改显示名。
fn doc_rename(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let name = p
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("名称不能为空".into());
    }
    if name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
        return Err("名称不能包含 \\ / : * ? \" < > |".into());
    }

    let entry = docs.get_mut(&id)?;
    if entry.path.is_empty() {
        entry.name = name.clone();
        return Ok(json!({ "docId": id, "name": entry.name, "path": entry.path }));
    }

    let old = std::path::PathBuf::from(&entry.path);
    let new_path = old
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(&name);
    if new_path != old {
        if new_path.exists() {
            return Err(format!("目标已存在：{}", new_path.display()));
        }
        std::fs::rename(&old, &new_path).map_err(|e| format!("重命名失败: {e}"))?;
    }
    entry.name = name;
    entry.path = new_path.to_string_lossy().into_owned();
    Ok(json!({ "docId": id, "name": entry.name, "path": entry.path }))
}

/// 「保存」/「另存为…」/「导出…」三合一。
///
/// - 无 `path` → 覆盖写回原文件（「保存」；内存文档报错提示另存为）
/// - 有 `path` → 写到新文件并把文档改绑过去（「另存为…」）
/// - 有 `begin`/`end` → 只写该区间（「导出…」选区）
/// - `format` 缺省时按目标后缀推断
fn doc_save(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let id = doc_id_of(p);
    let mut opts = EncodeOptions::from_params(p);

    // 1) 目标路径 + 源缓冲（短借一瞬，别把锁拿着做编码）
    let (path, src) = {
        let entry = docs.get(&id)?;
        let path = match p
            .get("path")
            .and_then(|x| x.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(s) => std::path::PathBuf::from(s),
            None => {
                if entry.path.is_empty() {
                    return Err("该文档没有关联文件（录音产物），请用「另存为…」".into());
                }
                std::path::PathBuf::from(&entry.path)
            }
        };
        (path, entry.buf.clone())
    };
    if p.get("format").and_then(|x| x.as_str()).is_none() {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            opts.format = ext.trim_start_matches('.').to_ascii_lowercase();
        }
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() && !dir.is_dir() {
            return Err(format!("目录不存在：{}", dir.display()));
        }
    }

    // 2) 选区导出
    let frames = src.frames();
    let ch = src.channels.max(1);
    let begin = as_u64(p, "begin", 0) as usize;
    let end = as_u64(p, "end", frames as u64) as usize;
    let region = begin > 0 || end < frames;
    let mut out_buf: AudioBuffer = if region {
        if begin >= end || begin >= frames {
            return Err("导出区间为空".into());
        }
        let hi = end.min(frames);
        AudioBuffer {
            sample_rate: src.sample_rate,
            channels: ch,
            samples: src.samples[begin * ch..hi * ch].to_vec(),
        }
    } else {
        (*src).clone()
    };

    // 2.5) 导出时临时重采样（不影响源文档）
    if let Some(target_rate) = p.get("sampleRate").and_then(|x| x.as_u64()) {
        let target_rate = target_rate as u32;
        if target_rate != out_buf.sample_rate && (4_000..=384_000).contains(&target_rate) {
            out_buf = crate::transform::resample(&out_buf, target_rate);
        }
    }

    // 3) 编码写出
    let (bytes, via_ffmpeg) =
        crate::encode::encode_to_file(&path, &out_buf, &opts).map_err(|e| e.to_string())?;

    // 4) 另存为 → 文档改绑到新文件，并回读真实格式
    let new_path = path.to_string_lossy().into_owned();
    let rebound = {
        let entry = docs.get_mut(&id)?;
        let changed = entry.path != new_path;
        if changed {
            entry.path = new_path.clone();
            if let Some(n) = path.file_name() {
                entry.name = n.to_string_lossy().into_owned();
            }
            if let Ok(m) = crate::decode::probe_meta(&path) {
                entry.meta = m;
            }
        }
        changed
    };

    Ok(json!({
        "docId": id,
        "path": new_path,
        "bytes": bytes,
        "format": opts.format,
        "bitDepth": opts.bit_depth,
        "region": region,
        "frames": out_buf.frames(),
        "viaFfmpeg": via_ffmpeg,
        "rebound": rebound,
    }))
}

/// 逐声道波形峰值（ocenaudio 式每声道一行渲染的数据源）。
/// 返回 data = 逐声道顺次拼接的 [min, max] 平铺数组。
fn waveform_cmd(docs: &Docs, p: &Value) -> Result<Value, String> {
    let doc = docs.get(p.get("docId").and_then(|x| x.as_str()).unwrap_or(""))?;
    let ch = doc.buf.channels.max(1);
    let frames = doc.buf.frames();
    // 可选区间 [begin, end)（帧）—— 缩放视图只取可见窗口
    let end = (as_u64(p, "end", frames as u64) as usize).min(frames);
    let begin = (as_u64(p, "begin", 0) as usize).min(end);
    let n = end - begin;
    // 桶数不超过区间帧数（否则多出来的桶是空的）；边界按比例切分，不留尾部空桶
    let buckets = (as_u64(p, "buckets", 2000) as usize).clamp(1, 16384).min(n.max(1));
    let mut data = Vec::with_capacity(ch * buckets * 2);
    for c in 0..ch {
        for b in 0..buckets {
            let f0 = begin + b * n / buckets;
            let f1 = (begin + (b + 1) * n / buckets).max(f0 + 1).min(end);
            let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
            let mut i = f0 * ch + c;
            let stop = f1 * ch;
            while i < stop {
                let v = doc.buf.samples[i];
                mn = mn.min(v);
                mx = mx.max(v);
                i += ch;
            }
            if mn > mx {
                (mn, mx) = (0.0, 0.0);
            }
            data.push(mn);
            data.push(mx);
        }
    }
    Ok(json!({
        "channels": ch,
        "buckets": buckets,
        "begin": begin,
        "end": end,
        "data": data,
    }))
}

fn spectrogram_cmd(docs: &Docs, p: &Value) -> Result<Value, String> {
    let doc = docs.get(p.get("docId").and_then(|x| x.as_str()).unwrap_or(""))?;
    let fft = as_u64(p, "fft", 1024) as usize;
    let hop = as_u64(p, "hop", 256) as usize;
    let win = p
        .get("window")
        .and_then(|x| x.as_str())
        .and_then(WindowType::from_config)
        .unwrap_or(WindowType::Hann);
    let dyn_db = as_f64(p, "dynRange", 80.0) as f32;
    let begin = as_u64(p, "begin", 0) as usize;
    let end = (as_u64(p, "end", doc.buf.frames() as u64) as usize).min(doc.buf.frames());
    let max_frames = as_u64(p, "maxFrames", 1500) as usize;
    let ch = doc.buf.channels.max(1);

    let lo = (begin * ch).min(doc.buf.samples.len());
    let hi = (end * ch).min(doc.buf.samples.len());
    // 声道选择：channel=N 取第 N 声道；否则正确混音（交错帧平均，而非把
    // 交错样本流误当 mono —— 那会让双声道频谱出现镜像伪影）。
    let mono: Vec<f32> = match p.get("channel").and_then(|x| x.as_u64()) {
        Some(ci) => {
            let ci = (ci as usize).min(ch - 1);
            (lo + ci..hi)
                .step_by(ch)
                .map(|i| doc.buf.samples[i])
                .collect()
        }
        None if ch == 1 => doc.buf.samples[lo..hi].to_vec(),
        None => doc.buf.samples[lo..hi]
            .chunks_exact(ch)
            .map(|fr| fr.iter().sum::<f32>() / ch as f32)
            .collect(),
    };

    // 默认输出绝对电平（dBFS，满幅正弦 ≈ 0 dB），不做逐帧归一化：同一电平在
    // 时间、缩放、声道之间颜色一致，静音/丢包缺口呈暗色。normalize=true 恢复旧的
    // 逐帧归一化（每帧峰值 0 dB）。
    let normalize = p.get("normalize").and_then(|x| x.as_bool()).unwrap_or(false);
    let cfg = SpectralConfig {
        fft_len: fft,
        window_size: fft,
        window_type: win,
        dyn_range_db: if normalize { dyn_db } else { 240.0 },
        normalize,
        pre_emphasis: false,
    };
    // hop 自适应：帧数超限则放大（保持总时长覆盖）
    let eff_hop = hop.max((mono.len().saturating_sub(fft)) / max_frames.max(1) + 1);
    let mut frames = spectrogram(&mono, &cfg, eff_hop)
        .into_iter()
        .map(|f| f.db)
        .collect::<Vec<_>>();
    // 本次请求范围内的最大电平（前端以全文件各声道的最大值作为配色参考）
    let max_db = frames
        .iter()
        .flat_map(|f| f.iter().copied())
        .fold(f32::NEG_INFINITY, f32::max);
    let max_db = if max_db.is_finite() { max_db } else { -240.0 };
    // 绝对模式下在 参考电平 − 动态范围 处裁剪（参考 = refDb 参数，缺省为本次最大值）。
    // 仅影响低于显示底的值，前端按同一 refDb/dynRange 着色，裁剪处即纯黑。
    let ref_db = p.get("refDb").and_then(|x| x.as_f64()).map(|v| v as f32).unwrap_or(max_db);
    let floor_db = if normalize { -dyn_db } else { ref_db - dyn_db };
    if !normalize {
        for f in frames.iter_mut() {
            for v in f.iter_mut() {
                *v = v.max(floor_db);
            }
        }
    }
    Ok(json!({
        "fft": fft, "hop": eff_hop,
        "sampleRate": doc.buf.sample_rate,
        "begin": begin,
        "frames": frames,
        "normalized": normalize,
        "maxDb": max_db,
        "floorDb": floor_db,
        "dynRange": dyn_db,
    }))
}

fn constellation_cmd(docs: &Docs, p: &Value) -> Result<Value, String> {
    let doc = docs.get(p.get("docId").and_then(|x| x.as_str()).unwrap_or(""))?;
    let cfg = ConstellationConfig {
        spectral: SpectralConfig {
            fft_len: as_u64(p, "fft", 1024) as usize,
            window_size: as_u64(p, "fft", 1024) as usize,
            window_type: WindowType::Hann,
            dyn_range_db: as_f64(p, "dynRange", 80.0) as f32,
            normalize: true,
            pre_emphasis: p.get("preEmphasis").and_then(|x| x.as_bool()).unwrap_or(false),
        },
        hop: as_u64(p, "hop", 64) as usize,
        peaks_per_frame: as_u64(p, "peaksPerFrame", 5) as usize,
        min_bin: as_u64(p, "minBin", 4) as usize,
        fan_out_frames: as_u64(p, "fanOut", 10) as u32,
        max_pairs: as_u64(p, "maxPairs", 5) as usize,
    };
    let begin = as_u64(p, "begin", 0) as usize;
    let end = (as_u64(p, "end", doc.buf.frames() as u64) as usize).min(doc.buf.frames());
    let ch = doc.buf.channels.max(1);
    let lo = (begin * ch).min(doc.buf.samples.len());
    let hi = (end * ch).min(doc.buf.samples.len());
    let mono = doc.buf.samples[lo..hi].to_vec();
    let res = constellation(&mono, doc.buf.sample_rate, &cfg);
    Ok(serde_json::to_value(res).map_err(|e| e.to_string())?)
}

fn pcm(docs: &Docs, p: &Value) -> Result<Value, String> {
    let doc = docs.get(p.get("docId").and_then(|x| x.as_str()).unwrap_or(""))?;
    let ch = doc.buf.channels.max(1);
    let begin = as_u64(p, "begin", 0) as usize;
    let end = (as_u64(p, "end", doc.buf.frames() as u64) as usize).min(doc.buf.frames());
    let lo = (begin * ch).min(doc.buf.samples.len());
    let hi = (end * ch).min(doc.buf.samples.len());
    let mut bytes = Vec::with_capacity((hi - lo) * 4);
    for v in &doc.buf.samples[lo..hi] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    Ok(json!({
        "sampleRate": doc.buf.sample_rate,
        "channels": ch,
        "begin": begin,
        "frames": end - begin,
        "f32leBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

/// 「波形和星座图」：rust_mss emd-core 0.3.0 default profile 指纹（fast peaks + 查询哈希上限）。
/// 峰 / 哈希用平铺数组：peaks = [t, f, dB, ...]，hashes = [t, f1, f2, dt, ...]；
/// f 为频带内下标（绝对 bin = f + binLo），帧 t 的中心 = t * hop / sampleRate 秒。
fn mss_constellation(docs: &Docs, p: &Value) -> Result<Value, String> {
    use crate::mss;
    let doc = docs.get(&doc_id_of(p))?;
    // 指纹计算是 O(全长 × FFT)：整段重采样 + STFT，长文件会耗尽内存/时间。
    // 只对「请求的时间窗」计算，并设硬上限。
    const MAX_SECONDS: f64 = 600.0; // 10 分钟
    let dur = doc.buf.duration_secs();
    let begin = as_f64(p, "begin", 0.0).max(0.0);
    let end = as_f64(p, "end", dur).min(dur);
    if !(end > begin) {
        return Err("时间范围为空".into());
    }
    if end - begin > MAX_SECONDS {
        return Err(format!(
            "时间窗过长（{:.0}s > 上限 {:.0}s），请缩小范围",
            end - begin,
            MAX_SECONDS
        ));
    }
    // 裁剪到窗口：按「帧」算，再乘声道数换成交错样本下标
    // （只按 秒×采样率 切的话，立体声只取到前一半样本、且声道会错位）
    let sr = doc.buf.sample_rate.max(1);
    let ch = doc.buf.channels.max(1);
    let frames = doc.buf.frames();
    let b = ((begin * sr as f64) as usize).min(frames);
    let e = ((end * sr as f64) as usize).min(frames).max(b);
    let seg = &doc.buf.samples[b * ch..e * ch];
    // channel=N：只算第 N 声道；缺省：混合单声道
    let channel = p.get("channel").and_then(|x| x.as_u64()).map(|c| c as usize);
    let clipped = match channel {
        Some(c) if c >= ch => return Err(format!("声道 {c} 不存在（共 {ch} 声道）")),
        Some(c) => AudioBuffer {
            sample_rate: doc.buf.sample_rate,
            channels: 1,
            samples: seg.iter().skip(c).step_by(ch).copied().collect(),
        },
        None => AudioBuffer { sample_rate: doc.buf.sample_rate, channels: ch, samples: seg.to_vec() },
    };
    let y = mss::to_8k_mono(&clipped);
    let fp = mss::fingerprint(&y);
    // 载荷上限：避免百万级数组跨 IPC 传输（前端只画可视窗口内的点）
    const MAX_PEAKS: usize = 200_000;
    const MAX_HASHES: usize = 200_000;
    let peak_iter = fp.peaks.iter().take(MAX_PEAKS);
    let mut peaks = Vec::with_capacity(peak_iter.size_hint().0 * 3);
    for pk in peak_iter {
        peaks.push(pk.t as f64);
        peaks.push(pk.f as f64);
        peaks.push((pk.db() as f64 * 100.0).round() / 100.0);
    }
    let hash_iter = fp.hashes.iter().take(MAX_HASHES);
    let mut hashes = Vec::with_capacity(hash_iter.size_hint().0 * 4);
    for h in hash_iter {
        hashes.extend_from_slice(&[h.t, h.f1 as u32, h.f2 as u32, h.dt as u32]);
    }
    Ok(json!({
        "algorithm": "rust_mss emd-core 0.3.0 default",
        "params": "fft=512,win=480,hop=80,lo=4,hi=144,ff=6,ft=7,thr=-6.25,zone=20,f2lo=1,f2hi=90; fast-peaks on; hash-cap 1500/s:20",
        "sampleRate": mss::SAMPLE_RATE,
        "hop": mss::HOP,
        "nFft": mss::N_FFT,
        "binLo": mss::BIN_LO,
        "binHi": mss::BIN_HI,
        "frames": fp.frames,
        "peaks": peaks,
        "hashes": hashes,
        "peaksUncapped": fp.peaks_uncapped,
        "hashesUncapped": fp.hashes_uncapped,
        "peaksTruncated": fp.peaks.len() > MAX_PEAKS,
        "hashesTruncated": fp.hashes.len() > MAX_HASHES,
        "beginSec": begin,
        "endSec": end,
        "channel": channel,
    }))
}

/// 「查看丢包率」。音频文件里没有真实的包信息（RTP 序号 / 时间戳），只能估算。
///
/// 丢包在解码后的音频里表现为「语音突然断掉一小段」。无损格式里断掉的是精确的 0，
/// 但 MP3 等有损格式解码后几乎没有精确的 0（实测缺口在 −44…−72 dBFS），所以按
/// **电平骤降**检测，而不是找 0：
/// 1. 每 5 ms 一帧算 RMS（dBFS）；不指定声道时取各声道中最响的（所有声道都断才算断）；
/// 2. 语音电平 = max(区间内 95 分位电平 − 20 dB, −60 dBFS)；
/// 3. 缺口 = 一段连续帧，比左右两侧 20 ms 内的语音峰值都低 ≥ 25 dB，且长 15…maxGapMs（300）ms；
///    更长的视为正常停顿；
/// 4. 每个缺口按 max(1, round(时长 / 包长)) 计丢包；总包数 = 首个到最后一个语音帧的时长 / 包长。
///
/// 参数在合成数据上标定：语音里挖 10 个 20/40/60 ms 缺口后编码为 8 kHz 32 kbps MP3，
/// 全部检出、干净 MP3 上 0 误报。
fn doc_packet_loss(docs: &Docs, p: &Value) -> Result<Value, String> {
    const HOP_MS: usize = 5;
    const DEPTH_DB: f64 = 25.0;
    const CONTEXT_MS: usize = 20;
    const SPEECH_BELOW_P95_DB: f64 = 20.0;
    const MIN_GAP_MS: usize = 15;
    const SPEECH_FLOOR_DB: f64 = -60.0;

    let doc = docs.get(&doc_id_of(p))?;
    let sr = doc.buf.sample_rate.max(1) as usize;
    let ch = doc.buf.channels.max(1);
    let packet_ms = as_u64(p, "packetMs", 20).clamp(5, 200) as usize;
    let max_gap_ms = as_u64(p, "maxGapMs", 300).max(packet_ms as u64) as usize;
    let frames = doc.buf.frames();
    // 可选：只看区间 [begin, end)（帧）、只看某个声道
    let re = (as_u64(p, "end", frames as u64) as usize).min(frames);
    let rb = (as_u64(p, "begin", 0) as usize).min(re);
    let channel = p.get("channel").and_then(|x| x.as_u64()).map(|c| c as usize);
    if let Some(c) = channel {
        if c >= ch {
            return Err(format!("声道 {c} 不存在（共 {ch} 声道）"));
        }
    }

    // 1. 5 ms 电平
    let hop = (sr * HOP_MS / 1000).max(1);
    let chans: Vec<usize> = match channel {
        Some(c) => vec![c],
        None => (0..ch).collect(),
    };
    let lv: Vec<f64> = (rb..re)
        .step_by(hop)
        .map(|f0| {
            let f1 = (f0 + hop).min(re);
            chans
                .iter()
                .map(|&c| {
                    let e: f64 = (f0..f1).map(|f| (doc.buf.samples[f * ch + c] as f64).powi(2)).sum();
                    10.0 * (e / (f1 - f0) as f64).max(1e-20).log10()
                })
                .fold(f64::NEG_INFINITY, f64::max)
        })
        .collect();
    let n = lv.len();
    let mut sorted = lv.clone();
    sorted.sort_by(f64::total_cmp);
    // 下限 −60 dBFS：全静音 / 只有底噪的区间不算语音
    let speech = (sorted.get(n * 95 / 100).copied().unwrap_or(-200.0) - SPEECH_BELOW_P95_DB).max(SPEECH_FLOOR_DB);
    let is_speech = |i: usize| lv[i] >= speech;

    // 2. 找缺口
    let k = CONTEXT_MS / HOP_MS;
    let max_run = max_gap_ms / HOP_MS;
    let ctx_max = |a: usize, b: usize| lv[a..b.min(n)].iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut gaps = Vec::new();
    let (mut lost, mut longest_ms) = (0usize, 0usize);
    let mut i = 0;
    while i < n {
        let left = ctx_max(i.saturating_sub(k), i);
        if i > 0 && left >= speech && lv[i] <= left - DEPTH_DB {
            let mut j = i;
            while j < n && lv[j] <= left - DEPTH_DB && j - i <= max_run {
                j += 1;
            }
            let right = ctx_max(j, j + k);
            let dur_ms = (j - i) * HOP_MS;
            let floor = lv[i..j].iter().copied().fold(f64::INFINITY, f64::min);
            if j < n && dur_ms >= MIN_GAP_MS && dur_ms <= max_gap_ms && right >= speech && floor <= right - DEPTH_DB {
                let pk = ((dur_ms as f64 / packet_ms as f64).round() as usize).max(1);
                lost += pk;
                longest_ms = longest_ms.max(dur_ms);
                gaps.push(json!({
                    "startSec": (rb + i * hop) as f64 / sr as f64,
                    "durationMs": dur_ms,
                    "packets": pk,
                    "depthDb": ((left.min(right) - floor) * 10.0).round() / 10.0,
                }));
                i = j;
                continue;
            }
        }
        i += 1;
    }

    // 3. 总包数：首个到最后一个语音帧
    let total = match ((0..n).find(|&i| is_speech(i)), (0..n).rev().find(|&i| is_speech(i))) {
        (Some(a), Some(b)) => (((b + 1 - a) * HOP_MS).div_ceil(packet_ms)).max(lost),
        _ => 0,
    };
    Ok(json!({
        "estimated": true,
        "method": "电平骤降估算（文件中没有真实的包信息）",
        "packetMs": packet_ms,
        "maxGapMs": max_gap_ms,
        "depthDb": DEPTH_DB,
        "lostPackets": lost,
        "totalPackets": total,
        "lossRate": if total > 0 { lost as f64 / total as f64 } else { 0.0 },
        "gapCount": gaps.len(),
        "longestGapMs": longest_ms,
        "gaps": gaps,
        "beginSec": rb as f64 / sr as f64,
        "endSec": re as f64 / sr as f64,
        "channel": channel,
    }))
}

/// 「抽取声道」：把第 `channel` 声道（可选区间 [begin, end) 帧）复制成一个新的单声道
/// 内存文档（未关联磁盘文件，需另存为）。原文档不变。
fn doc_extract_channel(docs: &mut Docs, p: &Value) -> Result<Value, String> {
    let src = docs.get(&doc_id_of(p))?;
    let ch = src.buf.channels.max(1);
    let c = p
        .get("channel")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| "channel required".to_string())? as usize;
    if c >= ch {
        return Err(format!("声道 {c} 不存在（共 {ch} 声道）"));
    }
    let (b, e) = span_of(p, src.buf.frames())?;
    let samples: Vec<f32> = src.buf.samples[b * ch..e * ch].iter().skip(c).step_by(ch).copied().collect();
    let stem = Path::new(&src.name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| src.name.clone());
    let tag = match (ch, c) {
        (2, 0) => "L".to_string(),
        (2, 1) => "R".to_string(),
        _ => format!("ch{c}"),
    };
    let range = if b > 0 || e < src.buf.frames() {
        let sr = src.buf.sample_rate.max(1) as f64;
        format!("_{:.2}-{:.2}s", b as f64 / sr, e as f64 / sr)
    } else {
        String::new()
    };
    let name = format!("{stem}_{tag}{range}.wav");
    let buf = Arc::new(AudioBuffer { sample_rate: src.buf.sample_rate, channels: 1, samples });
    let meta = src.meta.clone();
    let (resp, entry) = build_doc_entry(name, String::new(), buf, meta);
    docs.inner.insert(resp["docId"].as_str().unwrap().to_string(), entry);
    Ok(resp)
}
