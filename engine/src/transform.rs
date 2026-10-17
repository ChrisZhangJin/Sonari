//! 信号变换 —— 声道布局转换 + 采样率转换。
//!
//! 对标 ocenaudio「转换为」菜单（转换声道 / 转换采样率）。两者都是**原地
//! 改文档**（不改文件名、不改路径），时长保持不变：
//! - 采样率转换后帧数按比例缩放（8 kHz → 44.1 kHz 帧数 ×5.5125）
//! - 声道转换后帧数不变，样本布局重排
//!
//! ocenaudio 的采样率转换走 libdspb 的 SoundTouch / 重采样器；这里用
//! **加窗 sinc 插值 + 抗混叠低通**自实现，不引入外部依赖。

use std::f64::consts::PI;

use crate::decode::AudioBuffer;

/// sinc 核的基础半宽（单侧 taps 数）。
const HALF_TAPS: i64 = 16;
/// 降采样时核可以放大（按 1/cutoff 补偿），但设上限以免时长 ×taps 爆掉。
const MAX_HALF_TAPS: i64 = 48;

/// 归一化 sinc 插值核（Hann 窗），`cutoff` 为归一化截止频率（≤1.0）。
///
/// 核半宽随 cutoff 收缩而放大：降采样时截止频率被压到 ratio，若仍用固定的
/// 16 点核，一个 sinc 主瓣只摊到 5 个源样本，阻带衰减掉到 -40 dB 量级。
#[inline]
fn kernel(t: f64, cutoff: f64, half: i64) -> f64 {
    if t.abs() > half as f64 {
        return 0.0;
    }
    let x = PI * t * cutoff;
    let sinc = if x.abs() < 1e-9 { 1.0 } else { x.sin() / x };
    // Hann 窗：让核在 ±half 处平滑归零，压制截断旁瓣
    let w = 0.5 + 0.5 * (PI * t / half as f64).cos();
    sinc * w * cutoff
}

/// 采样率转换。
///
/// - `new_rate == old_rate` → 原样克隆（避免无意义的重算）
/// - 升采样时 cutoff = 1.0（保留全带宽）；降采样时 cutoff = ratio，
///   以新奈奎斯特频率为界做抗混叠，避免折叠失真
pub fn resample(buf: &AudioBuffer, new_rate: u32) -> AudioBuffer {
    let old_rate = buf.sample_rate;
    let ch = buf.channels.max(1);
    let in_frames = buf.frames();

    if new_rate == 0 || old_rate == 0 || in_frames == 0 || new_rate == old_rate {
        return AudioBuffer {
            sample_rate: if new_rate == 0 { old_rate } else { new_rate },
            channels: ch,
            samples: buf.samples.clone(),
        };
    }

    let ratio = new_rate as f64 / old_rate as f64;
    let out_frames = ((in_frames as f64) * ratio).round() as usize;
    let cutoff = if ratio < 1.0 { ratio } else { 1.0 };
    let half = ((HALF_TAPS as f64 / cutoff).ceil() as i64).clamp(HALF_TAPS, MAX_HALF_TAPS);

    let mut out = vec![0.0f32; out_frames * ch];
    for i in 0..out_frames {
        // 输出帧 i 对应的源位置（浮点帧号）
        let pos = i as f64 / ratio;
        let base = pos.floor() as i64;
        let lo = base - half + 1;
        let hi = base + half;

        for c in 0..ch {
            let mut acc = 0.0f64;
            let mut wsum = 0.0f64;
            for k in lo..=hi {
                // 帧索引越界（首尾半个核宽）直接跳过，并把它从归一化里剔除，
                // 否则边缘会出现音量凹陷
                if k < 0 || k as usize >= in_frames {
                    continue;
                }
                let w = kernel(k as f64 - pos, cutoff, half);
                acc += buf.samples[k as usize * ch + c] as f64 * w;
                wsum += w;
            }
            out[i * ch + c] = if wsum.abs() > 1e-12 {
                (acc / wsum) as f32
            } else {
                0.0
            };
        }
    }

    AudioBuffer {
        sample_rate: new_rate,
        channels: ch,
        samples: out,
    }
}

/// 声道布局转换。
///
/// - 多声道 → 单声道：帧内平均（与 `AudioBuffer::to_mono` 一致，不会削顶）
/// - 单声道 → 立体声：复制到左右两声道
/// - 其它情况：目标声道数与原值相同时原样返回；不同时按「取前 N / 补零」处理
pub fn convert_channels(buf: &AudioBuffer, target: usize) -> AudioBuffer {
    let src_ch = buf.channels.max(1);
    let dst_ch = target.max(1);
    if src_ch == dst_ch {
        return AudioBuffer {
            sample_rate: buf.sample_rate,
            channels: dst_ch,
            samples: buf.samples.clone(),
        };
    }

    let frames = buf.frames();
    let mut out = vec![0.0f32; frames * dst_ch];

    match (src_ch, dst_ch) {
        // 下混到单声道
        (_, 1) => {
            for f in 0..frames {
                let seg = &buf.samples[f * src_ch..(f + 1) * src_ch];
                out[f] = seg.iter().sum::<f32>() / src_ch as f32;
            }
        }
        // 单声道上混到多声道：每个声道都复制同一份
        (1, _) => {
            for f in 0..frames {
                let v = buf.samples[f];
                for c in 0..dst_ch {
                    out[f * dst_ch + c] = v;
                }
            }
        }
        // 其它布局：能拿到的直接搬，缺的补零
        _ => {
            for f in 0..frames {
                for c in 0..dst_ch {
                    out[f * dst_ch + c] = if c < src_ch {
                        buf.samples[f * src_ch + c]
                    } else {
                        0.0
                    };
                }
            }
        }
    }

    AudioBuffer {
        sample_rate: buf.sample_rate,
        channels: dst_ch,
        samples: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, secs: f64, ch: usize) -> AudioBuffer {
        let frames = (rate as f64 * secs) as usize;
        let mut samples = Vec::with_capacity(frames * ch);
        for f in 0..frames {
            let v = (2.0 * PI * hz * f as f64 / rate as f64).sin() as f32 * 0.5;
            for _ in 0..ch {
                samples.push(v);
            }
        }
        AudioBuffer {
            sample_rate: rate,
            channels: ch,
            samples,
        }
    }

    /// 过零率测频：纯正弦下过零点数 / 2 / 时长即为频率。
    /// 掐掉首尾各 5% 再统计，绕开 sinc 核在边界处的斜坡。
    fn dominant_hz(buf: &AudioBuffer) -> f64 {
        let mono = buf.to_mono();
        let n = mono.len();
        let (a, b) = (n / 20, n - n / 20);
        let seg = &mono[a..b];
        let mut crossings = 0usize;
        for w in seg.windows(2) {
            if (w[0] < 0.0) != (w[1] < 0.0) {
                crossings += 1;
            }
        }
        crossings as f64 / 2.0 / (seg.len() as f64 / buf.sample_rate as f64)
    }

    #[test]
    fn upsample_keeps_frequency_and_length() {
        let src = sine(8000, 440.0, 0.5, 1);
        let out = resample(&src, 44100);
        assert_eq!(out.sample_rate, 44100);
        assert_eq!(out.channels, 1);
        assert_eq!(out.frames(), 22050); // 0.5s @ 44.1k
        assert!((dominant_hz(&out) - 440.0).abs() < 6.0, "got {}", dominant_hz(&out));
    }

    #[test]
    fn downsample_keeps_frequency() {
        let src = sine(44100, 1000.0, 0.5, 2);
        let out = resample(&src, 16000);
        assert_eq!(out.sample_rate, 16000);
        assert_eq!(out.channels, 2);
        assert_eq!(out.frames(), 8000);
        assert!((dominant_hz(&out) - 1000.0).abs() < 12.0, "got {}", dominant_hz(&out));
    }

    #[test]
    fn resample_same_rate_is_clone() {
        let src = sine(44100, 440.0, 0.1, 1);
        let out = resample(&src, 44100);
        assert_eq!(out.samples, src.samples);
    }

    #[test]
    fn downmix_to_mono_averages() {
        let buf = AudioBuffer {
            sample_rate: 100,
            channels: 2,
            samples: vec![1.0, 0.5, -1.0, -0.5],
        };
        let out = convert_channels(&buf, 1);
        assert_eq!(out.channels, 1);
        assert_eq!(out.frames(), 2);
        assert!((out.samples[0] - 0.75).abs() < 1e-6);
        assert!((out.samples[1] + 0.75).abs() < 1e-6);
    }

    #[test]
    fn upmix_mono_to_stereo_duplicates() {
        let buf = AudioBuffer {
            sample_rate: 100,
            channels: 1,
            samples: vec![0.25, -0.25],
        };
        let out = convert_channels(&buf, 2);
        assert_eq!(out.channels, 2);
        assert_eq!(out.samples, vec![0.25, 0.25, -0.25, -0.25]);
    }

    #[test]
    fn stereo_to_stereo_is_clone() {
        let buf = AudioBuffer {
            sample_rate: 100,
            channels: 2,
            samples: vec![0.1, 0.2],
        };
        let out = convert_channels(&buf, 2);
        assert_eq!(out.samples, buf.samples);
    }
}
