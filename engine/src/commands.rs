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
}

#[derive(Default)]
pub struct Docs {
    inner: HashMap<String, DocEntry>,
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
    })
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
    let new_buf = Arc::new(crate::transform::resample(&entry.buf, rate));
    let (peaks, stats) = peaks_and_stats(&new_buf);
    entry.buf = new_buf;
    entry.peaks = peaks;
    Ok(entry_json(&id, entry, stats))
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
    let new_buf = Arc::new(crate::transform::convert_channels(&entry.buf, target));
    let (peaks, stats) = peaks_and_stats(&new_buf);
    entry.buf = new_buf;
    entry.peaks = peaks;
    Ok(entry_json(&id, entry, stats))
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
    let out_buf: AudioBuffer = if region {
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
    let buckets = (as_u64(p, "buckets", 2000) as usize).clamp(64, 16384);
    let ch = doc.buf.channels.max(1);
    let frames = doc.buf.frames();
    let seg = frames.div_ceil(buckets).max(1);
    let mut data = Vec::with_capacity(ch * buckets * 2);
    for c in 0..ch {
        for b in 0..buckets {
            let f0 = (b * seg).min(frames);
            let f1 = (((b + 1) * seg).min(frames)).max(f0);
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

    let cfg = SpectralConfig {
        fft_len: fft,
        window_size: fft,
        window_type: win,
        dyn_range_db: dyn_db,
        normalize: true,
        pre_emphasis: false,
    };
    // hop 自适应：帧数超限则放大（保持总时长覆盖）
    let eff_hop = hop.max((mono.len().saturating_sub(fft)) / max_frames.max(1) + 1);
    let frames = spectrogram(&mono, &cfg, eff_hop)
        .into_iter()
        .map(|f| f.db)
        .collect::<Vec<_>>();
    Ok(json!({
        "fft": fft, "hop": eff_hop,
        "sampleRate": doc.buf.sample_rate,
        "begin": begin,
        "frames": frames,
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
