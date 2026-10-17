//! 解码层 —— 对标 `libiaudio.dll` 的 FormatFilter 家族（80 种）。
//!
//! 用 symphonia 覆盖日常格式的绝大多数：WAV/MP3/FLAC/OGG/AAC/ALAC/AIFF/W64/
//! Vorbis/Opus/PCM 全家。ocenaudio 另有 FFMPEG 兜底与 OCEN 工程格式，一期不做。
//!
//! 输出统一为交错 f32（-1.0..1.0），这是整个引擎的内部表示
//! （对应 ocenaudio 的 AUDIOSIGNAL 层）。

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::{Error as NsError, Result};

/// 解码后的音频，交错存储。
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: usize,
    /// 交错样本，len = frames * channels
    pub samples: Vec<f32>,
}

impl AudioBuffer {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels
        }
    }

    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames() as f64 / self.sample_rate as f64
        }
    }

    /// 取单声道（混合）。
    pub fn to_mono(&self) -> Vec<f32> {
        match self.channels {
            0 => Vec::new(),
            1 => self.samples.clone(),
            n => self
                .samples
                .chunks(n)
                .map(|c| c.iter().sum::<f32>() / n as f32)
                .collect(),
        }
    }
}

/// 解码整个文件到内存（API 按 symphonia 0.6 官方 basic-interleaved 示例）。
pub fn decode_file(path: &Path) -> Result<AudioBuffer> {
    Ok(decode_file_with_meta(path)?.0)
}

/// 带码流元数据的解码（分辨率 / 编解码器 —— 「查看信息」用）。
pub fn decode_file_with_meta(path: &Path) -> Result<(AudioBuffer, StreamMeta)> {
    let file = File::open(path).map_err(|e| NsError::Decode(format!("open: {e}")))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let hint = Hint::new();
    let fmt_opts: FormatOptions = Default::default();
    let meta_opts: MetadataOptions = Default::default();
    let dec_opts: AudioDecoderOptions = Default::default();

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, fmt_opts, meta_opts)
        .map_err(|e| NsError::Decode(format!("probe: {e}")))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| NsError::Decode("no audio track".into()))?;

    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| NsError::Decode("no audio codec params".into()))?;

    let sample_rate = codec_params.sample_rate;
    let channels = codec_params.channels.as_ref().map(|c| c.count());
    let meta = StreamMeta {
        bits_per_sample: codec_params.bits_per_sample,
        bits_per_coded_sample: codec_params.bits_per_coded_sample,
        codec: format!("{:?}", codec_params.codec),
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &dec_opts)
        .map_err(|e| NsError::Decode(format!("decoder: {e}")))?;

    let track_id = track.id;
    let mut samples: Vec<f32> = Vec::new();
    let mut chunk: Vec<f32> = Vec::new();
    let detected_channels: Option<usize> = None;

    while let Some(packet) = format
        .next_packet()
        .map_err(|e| NsError::Decode(format!("packet: {e}")))?
    {
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let n = buf.samples_interleaved();
                chunk.clear();
                chunk.resize(n, 0.0);
                buf.copy_to_slice_interleaved(&mut chunk);
                if detected_channels.is_none() {
                    // spec 从解码缓冲拿最准
                    let _ = &detected_channels;
                }
                samples.extend_from_slice(&chunk);
            }
            Err(Error::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    // 通道数兜底：优先 codec 参数，否则按解码数据推断（1/2 之外按立体声处理）
    let ch = channels
        .or(detected_channels)
        .unwrap_or(if samples.len() % 2 == 0 { 2 } else { 1 })
        .max(1);

    if samples.is_empty() {
        return Err(NsError::Decode("no samples decoded".into()));
    }
    let sr = sample_rate.ok_or_else(|| NsError::Decode("no sample rate".into()))?;

    Ok((
        AudioBuffer {
            sample_rate: sr,
            channels: ch,
            samples,
        },
        meta,
    ))
}

/// 只读码流头部（不解码）—— 「另存为」写出后回读真实格式用。
pub fn probe_meta(path: &Path) -> Result<StreamMeta> {
    let file = File::open(path).map_err(|e| NsError::Decode(format!("open: {e}")))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, Default::default(), Default::default())
        .map_err(|e| NsError::Decode(format!("probe: {e}")))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| NsError::Decode("no audio track".into()))?;
    let cp = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| NsError::Decode("no audio codec params".into()))?;
    Ok(StreamMeta {
        bits_per_sample: cp.bits_per_sample,
        bits_per_coded_sample: cp.bits_per_coded_sample,
        codec: format!("{:?}", cp.codec),
    })
}

/// 码流元数据（「查看信息」对话框的信号属性来源）。
#[derive(Debug, Clone, Default)]
pub struct StreamMeta {
    /// 解码后每样本有效位数（16/24/32…）
    pub bits_per_sample: Option<u32>,
    /// 编码位深（与解码位深不同时才有值）
    pub bits_per_coded_sample: Option<u32>,
    /// symphonia 的编解码器标识（Debug 串，如 PcmS16Le）
    pub codec: String,
}

impl StreamMeta {
    /// 录音产物：引擎内部表示就是交错 32 位浮点。
    pub fn pcm_f32() -> Self {
        use symphonia::core::codecs::audio::well_known as Id;
        Self {
            bits_per_sample: Some(32),
            bits_per_coded_sample: Some(32),
            codec: format!("{:?}", Id::CODEC_ID_PCM_F32LE),
        }
    }

    /// 编解码器标识 → 人类可读名（ocenaudio 信息框的「格式」行）。
    ///
    /// `AudioCodecId` 的字段是私有的，只有 Debug 串可读（`AudioCodecId(264)`），
    /// 因此拿常量的 Debug 串与它比对，避免依赖字面量数值。
    pub fn codec_label(&self) -> String {
        use symphonia::core::codecs::audio::{well_known as Id, AudioCodecId};
        const TABLE: &[(AudioCodecId, &str)] = &[
            (Id::CODEC_ID_PCM_U8, "PCM 8 位无符号"),
            (Id::CODEC_ID_PCM_S8, "PCM 8 位有符号"),
            (Id::CODEC_ID_PCM_U16LE, "PCM 16 位无符号（小端）"),
            (Id::CODEC_ID_PCM_S16LE, "PCM 16 位有符号（小端）"),
            (Id::CODEC_ID_PCM_S16BE, "PCM 16 位有符号（大端）"),
            (Id::CODEC_ID_PCM_U24LE, "PCM 24 位无符号（小端）"),
            (Id::CODEC_ID_PCM_S24LE, "PCM 24 位有符号（小端）"),
            (Id::CODEC_ID_PCM_S24BE, "PCM 24 位有符号（大端）"),
            (Id::CODEC_ID_PCM_U32LE, "PCM 32 位无符号（小端）"),
            (Id::CODEC_ID_PCM_S32LE, "PCM 32 位有符号（小端）"),
            (Id::CODEC_ID_PCM_S32BE, "PCM 32 位有符号（大端）"),
            (Id::CODEC_ID_PCM_F32LE, "PCM 32 位浮点"),
            (Id::CODEC_ID_PCM_F64LE, "PCM 64 位浮点"),
            (Id::CODEC_ID_PCM_ALAW, "A-law（G.711）"),
            (Id::CODEC_ID_PCM_MULAW, "μ-law（G.711）"),
            (Id::CODEC_ID_ADPCM_MS, "Microsoft ADPCM"),
            (Id::CODEC_ID_ADPCM_IMA_WAV, "IMA ADPCM (WAV)"),
            (Id::CODEC_ID_ADPCM_IMA_QT, "IMA ADPCM (QuickTime)"),
            (Id::CODEC_ID_ADPCM_G722, "G.722 ADPCM"),
            (Id::CODEC_ID_ADPCM_G726, "G.726 ADPCM"),
            (Id::CODEC_ID_VORBIS, "Ogg Vorbis"),
            (Id::CODEC_ID_OPUS, "Opus"),
            (Id::CODEC_ID_SPEEX, "Speex"),
            (Id::CODEC_ID_MP1, "MPEG-1/2 Layer I"),
            (Id::CODEC_ID_MP2, "MPEG-1/2 Layer II"),
            (Id::CODEC_ID_MP3, "MPEG-1/2 Layer III（MP3）"),
            (Id::CODEC_ID_AAC, "AAC"),
            (Id::CODEC_ID_AC3, "Dolby AC-3"),
            (Id::CODEC_ID_EAC3, "Dolby E-AC-3"),
            (Id::CODEC_ID_FLAC, "FLAC（无损）"),
            (Id::CODEC_ID_ALAC, "Apple Lossless（ALAC）"),
            (Id::CODEC_ID_WAVPACK, "WavPack（无损）"),
            (Id::CODEC_ID_MONKEYS_AUDIO, "Monkey's Audio（无损）"),
            (Id::CODEC_ID_TTA, "True Audio（无损）"),
            (Id::CODEC_ID_WMA, "Windows Media Audio"),
        ];
        for (id, name) in TABLE {
            if self.codec == format!("{id:?}") {
                return (*name).to_string();
            }
        }
        // 未收录：把 Debug 串里的数值转成十六进制，便于对照 symphonia 常量表
        if let Some(inner) = self
            .codec
            .strip_prefix("AudioCodecId(")
            .and_then(|s| s.strip_suffix(')'))
            .and_then(|s| s.parse::<u32>().ok())
        {
            return format!("未知编解码器 (0x{inner:04x})");
        }
        if self.codec.is_empty() {
            "未知".to_string()
        } else {
            self.codec.clone()
        }
    }
}
