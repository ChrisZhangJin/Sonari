//! 文档模型 —— 对标 `libocen.dll` 的 OCENAUDIO_ 组（902 API）。
//!
//! ocenaudio 的模型：Document → Track[] → Region[]（引用 AudioBlocks），
//! 外加 Selection[] / Marker / Undo。音频不是一整块内存而是块引用链，
//! 这是它 undo / 无损剪辑的基础（AUDIOBLOCKSTORE，17 个 API）。
//!
//! 一期从最小模型起步：单文档、多通道、选区、标记、undo（命令式）。
//! Region/Block 引用链在二期引入编辑功能时再上。

use serde::{Deserialize, Serialize};

/// 播放区间（帧号，半开区间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Span {
    pub begin: u64,
    pub end: u64,
}

impl Span {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.begin)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.begin
    }
}

/// 标记（对应 OCENAUDIO_AddMarkerToSample）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    pub frame: u64,
    pub label: String,
}

/// 文档头部信息（对应 GetSignalFormat / GetFile* 系列）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocInfo {
    pub name: String,
    pub path: Option<String>,
    pub sample_rate: u32,
    pub channels: usize,
    pub frames: u64,
    /// 峰值缓存是否已计算（对应 ocenaudio 的 AudioForm 后台线程产物）
    pub peaks_ready: bool,
}

/// 编辑命令 —— undo 栈的元素。
/// ocenaudio 用 OCENUNDO_AddRevert* 注册「逆操作」，这里用命令快照等价表达。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EditCmd {
    /// 剪掉 span 并把数据存进命令里（逆操作 = 踩回去）
    Cut { span: Span, data: Vec<f32>, channels: usize },
    /// 在 frame 处粘贴（逆操作 = 再剪掉）
    Paste { frame: u64, inserted: u64 },
    /// 增益（逆操作 = 反向增益）
    Gain { span: Span, factor: f32 },
    AddMarker { frame: u64 },
    RemoveMarker { index: usize, marker: Marker },
}

/// 一个音频文档（单 clip 起步）。
pub struct Document {
    pub info: DocInfo,
    pub samples: Vec<f32>,
    pub markers: Vec<Marker>,
    pub selection: Option<Span>,
    undo: Vec<EditCmd>,
    redo: Vec<EditCmd>,
}

impl Document {
    pub fn new(name: impl Into<String>, buf: crate::decode::AudioBuffer) -> Self {
        Self {
            info: DocInfo {
                name: name.into(),
                path: None,
                sample_rate: buf.sample_rate,
                channels: buf.channels,
                frames: buf.frames() as u64,
                peaks_ready: false,
            },
            samples: buf.samples,
            markers: Vec::new(),
            selection: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    fn frame_span(&self, span: Span) -> std::ops::Range<usize> {
        let ch = self.info.channels.max(1);
        let b = (span.begin as usize) * ch;
        let e = ((span.end as usize) * ch).min(self.samples.len());
        b.min(e)..e
    }

    pub fn cut(&mut self, span: Span) -> Option<EditCmd> {
        if span.is_empty() {
            return None;
        }
        let range = self.frame_span(span);
        let data = self.samples[range.clone()].to_vec();
        self.samples.drain(range);
        self.info.frames = self.samples.len() as u64 / self.info.channels.max(1) as u64;
        let cmd = EditCmd::Cut {
            span,
            data,
            channels: self.info.channels,
        };
        self.undo.push(cmd.clone());
        self.redo.clear();
        Some(cmd)
    }

    pub fn paste(&mut self, frame: u64, data: &[f32]) -> Option<EditCmd> {
        let ch = self.info.channels.max(1);
        let at = (frame as usize * ch).min(self.samples.len());
        self.samples
            .splice(at..at, data.iter().copied());
        let inserted = data.len() as u64 / ch as u64;
        self.info.frames = self.samples.len() as u64 / ch as u64;
        let cmd = EditCmd::Paste { frame, inserted };
        self.undo.push(cmd.clone());
        self.redo.clear();
        Some(cmd)
    }

    pub fn apply_gain(&mut self, span: Span, factor: f32) -> Option<EditCmd> {
        if span.is_empty() {
            return None;
        }
        let range = self.frame_span(span);
        for s in &mut self.samples[range] {
            *s *= factor;
        }
        let cmd = EditCmd::Gain { span, factor };
        self.undo.push(cmd.clone());
        self.redo.clear();
        Some(cmd)
    }

    pub fn add_marker(&mut self, frame: u64, label: impl Into<String>) {
        let m = Marker {
            frame,
            label: label.into(),
        };
        self.markers.push(m.clone());
        self.undo.push(EditCmd::AddMarker { frame });
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(cmd) = self.undo.pop() else { return false };
        // 先借用提取 redo 命令，再消费 cmd（Cut 的 data 会被移出）
        let redo_cmd = match &cmd {
            EditCmd::Cut { span, channels, .. } => Some(EditCmd::Cut {
                span: *span,
                data: Vec::new(),
                channels: *channels,
            }),
            EditCmd::Gain { span, factor } => Some(EditCmd::Gain {
                span: *span,
                factor: *factor,
            }),
            EditCmd::AddMarker { frame } => Some(EditCmd::AddMarker { frame: *frame }),
            _ => None,
        };
        match cmd {
            EditCmd::Cut {
                span,
                data,
                channels: _,
            } => {
                let ch = self.info.channels.max(1);
                let at = (span.begin as usize) * ch;
                self.samples.splice(at..at, data);
                self.info.frames = self.samples.len() as u64 / ch as u64;
            }
            EditCmd::Paste { frame, inserted } => {
                let ch = self.info.channels.max(1) as u64;
                let b = frame * ch;
                let e = (frame + inserted) * ch;
                let e = (e as usize).min(self.samples.len());
                self.samples.drain(b as usize..e);
                self.info.frames = self.samples.len() as u64 / ch;
            }
            EditCmd::Gain { span, factor } => {
                let inv = 1.0 / factor;
                let range = self.frame_span(span);
                for s in &mut self.samples[range] {
                    *s *= inv;
                }
            }
            EditCmd::AddMarker { frame } => {
                if let Some(i) = self.markers.iter().rposition(|m| m.frame == frame) {
                    self.markers.remove(i);
                }
            }
            EditCmd::RemoveMarker { index, marker } => {
                let i = index.min(self.markers.len());
                self.markers.insert(i, marker);
            }
        }
        if let Some(rc) = redo_cmd {
            self.redo.push(rc);
        }
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(cmd) = self.redo.pop() else { return false };
        match cmd.clone() {
            EditCmd::Cut { span, .. } => {
                self.cut(span);
            }
            EditCmd::Paste { frame, inserted } => {
                // redo 需要数据；命令里没存 —— 简化：redo 仅对 Cut/Gain/AddMarker 可靠
                let _ = (frame, inserted);
                self.undo.push(cmd);
                return false;
            }
            EditCmd::Gain { span, factor } => {
                self.apply_gain(span, factor);
            }
            EditCmd::AddMarker { frame } => {
                self.add_marker(frame, String::new());
            }
            EditCmd::RemoveMarker { index, marker } => {
                let _ = (index, marker);
            }
        }
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// 波形峰值抽取（min/max 对），对应 ocenaudio 的 AudioForm 缓存。
    /// 每个输出桶覆盖 `samples_per_bucket` 帧。
    pub fn peaks(&self, channel: usize, samples_per_bucket: usize) -> Vec<(f32, f32)> {
        let ch = self.info.channels.max(1);
        if channel >= ch || samples_per_bucket == 0 {
            return Vec::new();
        }
        let frames = self.samples.len() / ch;
        let mut out = Vec::with_capacity(frames / samples_per_bucket + 1);
        let mut start = 0;
        while start < frames {
            let end = (start + samples_per_bucket).min(frames);
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for f in start..end {
                let v = self.samples[f * ch + channel];
                if v < lo {
                    lo = v;
                }
                if v > hi {
                    hi = v;
                }
            }
            out.push((lo, hi));
            start = end;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        let buf = crate::decode::AudioBuffer {
            sample_rate: 8000,
            channels: 1,
            samples: (0..100).map(|i| i as f32 / 100.0).collect(),
        };
        Document::new("t", buf)
    }

    #[test]
    fn cut_undo_roundtrip() {
        let mut d = doc();
        assert!(d.cut(Span { begin: 10, end: 20 }).is_some());
        assert_eq!(d.info.frames, 90);
        assert!(d.undo());
        assert_eq!(d.info.frames, 100);
        assert_eq!(d.samples[15], 0.15);
    }

    #[test]
    fn gain_undo() {
        let mut d = doc();
        d.apply_gain(Span { begin: 0, end: 100 }, 0.5);
        assert!((d.samples[10] - 0.05).abs() < 1e-6);
        d.undo();
        assert!((d.samples[10] - 0.10).abs() < 1e-6);
    }

    #[test]
    fn peaks_bounds() {
        let d = doc();
        let p = d.peaks(0, 10);
        assert_eq!(p.len(), 10);
        assert_eq!(p[0], (0.0, 0.09));
        assert_eq!(p[9], (0.90, 0.99));
    }
}
