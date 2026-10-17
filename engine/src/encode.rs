//! 编码写出 —— 对标 ocenaudio FormatFilter 的写出侧。
//!
//! 两条路，与 ocenaudio 的架构一致：
//!
//! 1. **内建 WAV**（RIFF/WAVE）：PCM 16 / PCM 24 / IEEE float 32。
//!    零外部依赖，任何时候都能用——这是「保存」「另存为 WAV」的默认路径。
//! 2. **FFMPEG 兜底**：MP3 / FLAC / OGG / Opus / M4A(AAC) / AIFF / WMA。
//!    ocenaudio 同样把有损编码交给外挂 FFMPEG（`libiaudio` 只是封装）。
//!    ffmpeg 不在 PATH 时，这些格式在 UI 里被禁用而不是静默失败。
//!
//! 引擎内部表示恒为交错 f32（-1.0..1.0），所以写出前只做量化和封装。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::decode::AudioBuffer;
use crate::{Error as NsError, Result};

/// 写出参数。
#[derive(Debug, Clone)]
pub struct EncodeOptions {
    /// 目标格式（小写后缀：wav / mp3 / flac / ogg / opus / m4a / aiff / wma）
    pub format: String,
    /// WAV / AIFF 的位深：16 / 24 / 32（32 = IEEE 浮点）
    pub bit_depth: u16,
    /// 有损格式的码率（kbps），0 = 用编码器默认
    pub bitrate_kbps: u32,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            format: "wav".into(),
            bit_depth: 16,
            bitrate_kbps: 0,
        }
    }
}

impl EncodeOptions {
    pub fn from_params(p: &Value) -> Self {
        let d = Self::default();
        let format = p
            .get("format")
            .and_then(|x| x.as_str())
            .map(|s| s.trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or(d.format);
        let bit_depth = p
            .get("bitDepth")
            .and_then(|x| x.as_u64())
            .map(|v| v as u16)
            .filter(|v| matches!(v, 16 | 24 | 32))
            .unwrap_or(d.bit_depth);
        let bitrate_kbps = p
            .get("bitrateKbps")
            .and_then(|x| x.as_u64())
            .map(|v| v as u32)
            .unwrap_or(d.bitrate_kbps);
        Self {
            format,
            bit_depth,
            bitrate_kbps,
        }
    }
}

/// 内建支持的格式（无需 ffmpeg）。
pub const BUILTIN_FORMATS: &[&str] = &["wav"];

/// ffmpeg 能写的扩展编码格式（按扩展名）。
const FFMPEG_FORMATS: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "aac", "aiff", "wma", "caf"];

/// 探测系统 ffmpeg。ocenaudio 的 FFMPEG 兜底依赖外部可执行文件，这里同理：
/// 先看 `SONARI_FFMPEG` 环境变量（便于指定非 PATH 里的构建），再查 PATH。
pub fn ffmpeg_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SONARI_FFMPEG") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 能力清单 —— 前端据此启用/禁用「另存为」里的格式。
pub fn capabilities() -> Value {
    let ff = ffmpeg_path();
    let mut formats: Vec<Value> = BUILTIN_FORMATS
        .iter()
        .map(|f| json!({ "ext": f, "label": format_label(f), "builtin": true, "lossy": false }))
        .collect();
    formats.extend(FFMPEG_FORMATS.iter().map(|f| {
        json!({
            "ext": f,
            "label": format_label(f),
            "builtin": false,
            "lossy": is_lossy(f),
            "available": ff.is_some(),
        })
    }));
    json!({
        "ffmpeg": ff.is_some(),
        "ffmpegPath": ff.map(|p| p.to_string_lossy().into_owned()),
        "bitDepths": [16, 24, 32],
        "bitrates": [128, 160, 192, 256, 320],
        "formats": formats,
    })
}

/// 人类可读格式名（下拉框显示）。
pub fn format_label(ext: &str) -> String {
    match ext {
        "wav" => "WAV（PCM，无损）".into(),
        "mp3" => "MP3（有损）".into(),
        "flac" => "FLAC（无损）".into(),
        "ogg" => "OGG Vorbis（有损）".into(),
        "opus" => "Opus（有损）".into(),
        "m4a" | "aac" => "M4A / AAC（有损）".into(),
        "aiff" => "AIFF（PCM，无损）".into(),
        "wma" => "WMA（有损）".into(),
        "caf" => "CAF（Core Audio）".into(),
        other => other.to_ascii_uppercase(),
    }
}

fn is_lossy(ext: &str) -> bool {
    matches!(ext, "mp3" | "ogg" | "opus" | "m4a" | "aac" | "wma")
}

/// 写出入口：按格式自动选内建 WAV 或 ffmpeg。返回 (写出字节数, 是否走了 ffmpeg)。
pub fn encode_to_file(path: &Path, buf: &AudioBuffer, opts: &EncodeOptions) -> Result<(u64, bool)> {
    match opts.format.as_str() {
        "wav" => Ok((write_wav(path, buf, opts.bit_depth)?, false)),
        other => {
            if !FFMPEG_FORMATS.contains(&other) {
                return Err(NsError::Invalid(format!("不支持的输出格式: {other}")));
            }
            let ff = ffmpeg_path().ok_or_else(|| {
                NsError::Invalid(
                    "未找到 ffmpeg —— 该格式需要 FFMPEG 兜底。\
                     请把 ffmpeg 加入 PATH，或设置 SONARI_FFMPEG 指向可执行文件。"
                        .into(),
                )
            })?;
            Ok((write_via_ffmpeg(&ff, path, buf, opts)?, true))
        }
    }
}

// ---------- 内建 WAV ----------

/// 写 RIFF/WAVE。`bit_depth` = 16 / 24 / 32（32 走 IEEE float，format tag 3）。
pub fn write_wav(path: &Path, buf: &AudioBuffer, bit_depth: u16) -> Result<u64> {
    let ch = buf.channels.max(1) as u16;
    let sr = buf.sample_rate;
    let frames = buf.frames();
    let is_float = bit_depth == 32;
    let bytes_per_sample = (bit_depth / 8) as usize;
    let block_align = (bytes_per_sample * ch as usize) as u16;
    let data_len = (frames * ch as usize * bytes_per_sample) as u32;

    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    // RIFF 头
    f.write_all(b"RIFF")?;
    f.write_all(&(36u32 + data_len).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    // fmt 块
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&(if is_float { 3u16 } else { 1u16 }).to_le_bytes())?; // 1=PCM, 3=IEEE float
    f.write_all(&ch.to_le_bytes())?;
    f.write_all(&sr.to_le_bytes())?;
    f.write_all(&(sr * block_align as u32).to_le_bytes())?; // byte rate
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bit_depth.to_le_bytes())?;
    // data 块
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;

    let mut out = Vec::with_capacity(data_len as usize);
    for s in &buf.samples {
        // 先钳位，避免 +1.0 溢出成负值（PCM 有符号整数范围是 -1.0..1.0-1LSB）
        let v = s.clamp(-1.0, 1.0);
        match (bit_depth, is_float) {
            (32, true) => out.extend_from_slice(&v.to_le_bytes()),
            (16, _) => out.extend_from_slice(&(((v * 32767.0).round()) as i16).to_le_bytes()),
            (24, _) => {
                let q = (v * 8_388_607.0).round() as i32;
                let b = q.to_le_bytes();
                out.extend_from_slice(&b[0..3]);
            }
            _ => out.extend_from_slice(&v.to_le_bytes()),
        }
    }
    f.write_all(&out)?;
    f.flush()?;
    Ok(36 + 8 + data_len as u64)
}

// ---------- ffmpeg 兜底 ----------

/// 用 ffmpeg 转码：先把引擎的 f32 落成临时 WAV，再让 ffmpeg 输出目标格式。
fn write_via_ffmpeg(ff: &Path, path: &Path, buf: &AudioBuffer, opts: &EncodeOptions) -> Result<u64> {
    let tmp = std::env::temp_dir().join(format!(
        "sonari-enc-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    write_wav(&tmp, buf, 32)?; // 32 位浮点中转，保留全部动态范围

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        tmp.to_string_lossy().into_owned(),
    ];
    let br = opts.bitrate_kbps;
    match opts.format.as_str() {
        "mp3" => {
            args.extend(["-c:a".into(), "libmp3lame".into()]);
            if br > 0 {
                args.extend(["-b:a".into(), format!("{br}k")]);
            }
        }
        "flac" => args.extend(["-c:a".into(), "flac".into(), "-compression_level".into(), "5".into()]),
        "ogg" => {
            args.extend(["-c:a".into(), "libvorbis".into()]);
            if br > 0 {
                args.extend(["-b:a".into(), format!("{br}k")]);
            } else {
                args.extend(["-q:a".into(), "5".into()]);
            }
        }
        "opus" => args.extend(["-c:a".into(), "libopus".into(), "-b:a".into(), format!("{}k", if br > 0 { br } else { 128 })]),
        "m4a" | "aac" => {
            args.extend(["-c:a".into(), "aac".into()]);
            if br > 0 {
                args.extend(["-b:a".into(), format!("{br}k")]);
            }
        }
        "wma" => args.extend(["-c:a".into(), "wmav2".into(), "-b:a".into(), format!("{}k", if br > 0 { br } else { 192 })]),
        "aiff" => args.extend(["-c:a".into(), pcm_codec(opts.bit_depth).into()]),
        "caf" => args.extend(["-c:a".into(), pcm_codec(opts.bit_depth).into()]),
        _ => {}
    }
    args.push(path.to_string_lossy().into_owned());

    let mut cmd = Command::new(ff);
    cmd.args(&args);
    // Windows 下别弹控制台窗口（GUI 进程里调用必需）
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let out = cmd.output().map_err(|e| NsError::Other(format!("ffmpeg 启动失败: {e}")))?;
    let _ = std::fs::remove_file(&tmp);
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(NsError::Other(format!(
            "ffmpeg 转码失败（{}）: {}",
            out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            msg.trim().lines().last().unwrap_or("").to_string()
        )));
    }
    Ok(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
}

fn pcm_codec(bit_depth: u16) -> &'static str {
    match bit_depth {
        24 => "pcm_s24be",
        32 => "pcm_f32be",
        _ => "pcm_s16be",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp() -> AudioBuffer {
        AudioBuffer {
            sample_rate: 8000,
            channels: 2,
            samples: (0..2000)
                .map(|i| ((i % 100) as f32 / 100.0) * 0.5 - 0.25)
                .collect(),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sonari-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn wav_roundtrip_16bit() {
        let p = tmp("enc16.wav");
        let src = ramp();
        write_wav(&p, &src, 16).unwrap();
        let back = crate::decode::decode_file(&p).unwrap();
        assert_eq!(back.sample_rate, 8000);
        assert_eq!(back.channels, 2);
        assert_eq!(back.frames(), src.frames());
        // 16 位量化误差 < 1/32767
        for (a, b) in src.samples.iter().zip(back.samples.iter()) {
            assert!((a - b).abs() < 4e-5, "{a} vs {b}");
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_roundtrip_24bit() {
        let p = tmp("enc24.wav");
        let src = ramp();
        write_wav(&p, &src, 24).unwrap();
        let back = crate::decode::decode_file(&p).unwrap();
        for (a, b) in src.samples.iter().zip(back.samples.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_roundtrip_float32_is_exact() {
        let p = tmp("enc32f.wav");
        let src = ramp();
        write_wav(&p, &src, 32).unwrap();
        let back = crate::decode::decode_file(&p).unwrap();
        assert_eq!(back.samples, src.samples);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_header_size_matches_16bit() {
        let p = tmp("encsize.wav");
        let src = AudioBuffer {
            sample_rate: 44100,
            channels: 2,
            samples: vec![0.0; 44100 * 2],
        };
        let n = write_wav(&p, &src, 16).unwrap();
        assert_eq!(n, 44 + (44100 * 2 * 2) as u64); // 44 字节头 + data
        assert_eq!(std::fs::metadata(&p).unwrap().len(), n);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn default_options_are_wav_16() {
        let o = EncodeOptions::from_params(&serde_json::json!({}));
        assert_eq!(o.format, "wav");
        assert_eq!(o.bit_depth, 16);
        // 非法位深回落到默认
        let o2 = EncodeOptions::from_params(&serde_json::json!({"bitDepth": 7}));
        assert_eq!(o2.bit_depth, 16);
        // 大小写与前导点归一
        let o3 = EncodeOptions::from_params(&serde_json::json!({"format": ".MP3"}));
        assert_eq!(o3.format, "mp3");
    }

    #[test]
    fn unsupported_format_is_rejected() {
        let p = tmp("enc.xyz");
        let e = encode_to_file(&p, &ramp(), &EncodeOptions {
            format: "xyz".into(),
            ..Default::default()
        });
        assert!(e.is_err());
    }
}
