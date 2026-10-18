//! `sonari-engine` — 音频引擎，对标 ocenaudio 的引擎三件套。
//!
//! | ocenaudio | 本 crate |
//! |---|---|
//! | `libocen.dll`（OCENAUDIO_ 902 API：Document/Track/Region/Selection/Undo） | `doc` |
//! | `libiaudio.dll`（80 种 FormatFilter + AUDIOSIGNAL 数据层） | `decode` |
//! | `libdspb.dll`（FFT/窗/滤波/SoundTouch/响度统计） | `dsp` / `analysis` |
//!
//! 关键架构决策（从逆向分析继承）：**引擎层零 GUI 依赖**——ocenaudio 的 libocen
//! 只通过 OCENCANVAS_* 抽象绘制，Qt 只是后端之一。这里引擎输出纯数据
//! （波形峰值、频谱帧、星座哈希），渲染完全交给前端 Vue/Canvas。

pub mod analysis;
pub mod audio;
pub mod commands;
pub mod decode;
pub mod doc;
pub mod dsp;
pub mod encode;
pub mod transform;

/// 音频命令（cpal 设备 / 播放 / 录音）——走 `AudioState` 而非文档锁。
pub const AUDIO_CMDS: &[&str] = &[
    "audio_devices",
    "playback_start",
    "playback_status",
    "playback_stop",
    "record_start",
    "record_status",
    "record_stop",
];

/// 写命令（会改文档）——需要写锁。其余一律走读锁并发分析。
pub const WRITE_CMDS: &[&str] = &[
    "doc_open",
    "doc_open_data",
    "doc_close",
    "doc_resample",
    "doc_channels",
    "doc_rename",
    "doc_save",
    "doc_paste",
];

pub fn is_audio_cmd(cmd: &str) -> bool {
    AUDIO_CMDS.contains(&cmd)
}

pub fn is_write_cmd(cmd: &str) -> bool {
    WRITE_CMDS.contains(&cmd)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("decode: {0}")]
    Decode(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid: {0}")]
    Invalid(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
