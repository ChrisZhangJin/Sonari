//! 分析层：频谱图 + **星座图**（用户核心诉求）。
//!
//! ## 星座图（constellation）的来历
//!
//! `ocen_const.log` 是一次注入 ocenaudio 进程的 PoC 遗留（hook
//! `OCENAUDIO_GetSamples` 等导出，fft=1024 hop=64，产出 stars/hashes）。
//! 这条路数源自 Shazam 式音频指纹：
//!   1. 分帧加窗 → FFT → 取频谱峰值点（landmark）
//!   2. 每个锚点与后续若干锚点配对 → (f1, f2, Δt) 哈希
//! 星座图就是把 landmark 按时间×频率画在 2D 平面上。
//!
//! 独立工具里这条链全部进程内完成，不需要任何注入。

use serde::{Deserialize, Serialize};

use crate::dsp::{psd_to_db, FftCache, WindowType};

/// 频谱分析参数 —— 对应 ocenaudio 的 `libocen.spectral.custom.*` 配置组。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectralConfig {
    pub fft_len: usize,
    pub window_size: usize,
    pub window_type: WindowType,
    pub dyn_range_db: f32,
    pub normalize: bool,
    /// 预加重（ocenaudio: use_preenhp_filter）——星座图前端高通，抑制低频能量堆积
    pub pre_emphasis: bool,
}

impl Default for SpectralConfig {
    fn default() -> Self {
        Self {
            fft_len: 1024,
            window_size: 1024,
            window_type: WindowType::Hann,
            dyn_range_db: 80.0,
            normalize: true,
            pre_emphasis: false,
        }
    }
}

/// 一帧频谱（dB）。
pub struct SpectralFrame {
    /// 起始帧号
    pub frame: u64,
    pub db: Vec<f32>,
}

/// 逐帧计算频谱。hop 控制时间分辨率（PoC 实测 hop=64 @ 8kHz）。
pub fn spectrogram(samples: &[f32], cfg: &SpectralConfig, hop: usize) -> Vec<SpectralFrame> {
    let n = cfg.fft_len;
    if samples.len() < n || n == 0 {
        return Vec::new();
    }
    let win = cfg.window_type.generate(cfg.window_size.min(n));
    let mut fft = FftCache::new();
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; n];
    let mut prev = 0.0f32;

    let mut start = 0usize;
    let mut frame_no = 0u64;
    while start + n <= samples.len() {
        // 加窗（含可选预强调 y[t] = x[t] - α·x[t-1]）
        if cfg.pre_emphasis {
            for i in 0..n {
                let x = samples[start + i];
                buf[i] = (x - 0.95 * prev) * win.get(i).copied().unwrap_or(1.0);
                prev = x;
            }
        } else {
            for i in 0..n {
                buf[i] = samples[start + i] * win.get(i).copied().unwrap_or(1.0);
            }
        }
        let mag = fft.magnitude(&buf);
        let db = psd_to_db(&mag, cfg.normalize, cfg.dyn_range_db);
        out.push(SpectralFrame {
            frame: frame_no,
            db,
        });
        frame_no += 1;
        start += hop;
    }
    out
}

/// 星座点（landmark）：时间×频率平面上的显著峰。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstellationStar {
    /// 帧号（时间轴，帧 × hop / sr = 秒）
    pub frame: u32,
    /// FFT bin（频率轴）
    pub bin: u32,
    /// 幅度（dB，用于绘制亮度/半径）
    pub db: f32,
}

/// 指纹哈希：锚点 + 目标点 → (f1, f2, Δt)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintHash {
    pub f1: u32,
    pub f2: u32,
    pub dt: u32,
    pub t1: u32,
}

/// 星座图参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstellationConfig {
    pub spectral: SpectralConfig,
    pub hop: usize,
    /// 每帧保留的峰值数
    pub peaks_per_frame: usize,
    /// 频带下限 bin（跳过 DC/超低频）
    pub min_bin: usize,
    /// 锚点向后配对的目标帧数
    pub fan_out_frames: u32,
    /// 每个锚点最多配对数
    pub max_pairs: usize,
}

impl Default for ConstellationConfig {
    fn default() -> Self {
        Self {
            spectral: SpectralConfig::default(),
            // PoC 实测参数：fft=1024 hop=64
            hop: 64,
            peaks_per_frame: 5,
            min_bin: 4,
            fan_out_frames: 10,
            max_pairs: 5,
        }
    }
}

/// 星座图分析结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstellationResult {
    pub stars: Vec<ConstellationStar>,
    pub hashes: Vec<FingerprintHash>,
    pub fft_len: usize,
    pub hop: usize,
    pub frames_analyzed: u32,
}

/// 计算星座图：频谱 → 每帧局部极大值 → 配对哈希。
pub fn constellation(samples: &[f32], sample_rate: u32, cfg: &ConstellationConfig) -> ConstellationResult {
    let _ = sample_rate; // 频率标注在前端做（bin → Hz 需要 sr）
    let frames = spectrogram(samples, &cfg.spectral, cfg.hop);

    // 每帧取 top-K 局部极大
    let mut stars: Vec<ConstellationStar> = Vec::new();
    let mut frame_peaks: Vec<Vec<(u32, f32)>> = Vec::with_capacity(frames.len());

    for f in &frames {
        let db: &[f32] = &f.db;
        let mut cand: Vec<(u32, f32)> = db
            .iter()
            .copied()
            .enumerate()
            .skip(cfg.min_bin)
            .take(db.len().saturating_sub(cfg.min_bin + 1))
            .filter(|(i, v)| {
                let i = *i;
                let v = *v;
                i > 0 && i + 1 < db.len() && v > db[i - 1] && v >= db[i + 1]
            })
            .map(|(i, v)| (i as u32, v))
            .collect();
        cand.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        cand.truncate(cfg.peaks_per_frame);
        for (bin, d) in &cand {
            stars.push(ConstellationStar {
                frame: f.frame as u32,
                bin: *bin,
                db: *d,
            });
        }
        frame_peaks.push(cand);
    }

    // 配对哈希：锚点 (t1,f1) → 后续 fan_out_frames 内的 (t2,f2)
    let mut hashes = Vec::new();
    'outer: for (t1, peaks) in frame_peaks.iter().enumerate() {
        for (f1, _) in peaks {
            let mut pairs = 0usize;
            for dt in 1..=cfg.fan_out_frames {
                let t2 = t1 + dt as usize;
                if t2 >= frame_peaks.len() {
                    break;
                }
                for (f2, _) in &frame_peaks[t2] {
                    hashes.push(FingerprintHash {
                        f1: *f1,
                        f2: *f2,
                        dt: dt,
                        t1: t1 as u32,
                    });
                    pairs += 1;
                    if pairs >= cfg.max_pairs {
                        continue 'outer;
                    }
                }
            }
        }
    }

    ConstellationResult {
        stars,
        hashes,
        fft_len: cfg.spectral.fft_len,
        hop: cfg.hop,
        frames_analyzed: frames.len() as u32,
    }
}

/// 响度/峰值统计 —— 对标 libdspb 的 `DSPB_Statistics*`（BS.1770）。
/// ebur128 crate 封装了完整实现，这里只做便捷聚合。
pub struct LevelStats {
    pub integrated_lufs: f64,
    pub loudness_range_lu: f64,
    pub true_peak_dbtp: f64,
    pub sample_peak_dbfs: f64,
}

pub fn level_stats(samples: &[f32], sample_rate: u32, channels: usize) -> Option<LevelStats> {
    use ebur128::{EbuR128, Mode};
    if channels == 0 || sample_rate == 0 {
        return None;
    }
    let mut e = EbuR128::new(channels as u32, sample_rate, Mode::all()).ok()?;
    // ebur128 接口是交错 f32
    e.add_frames_f32(samples).ok()?;
    Some(LevelStats {
        integrated_lufs: e.loudness_global().ok()?,
        loudness_range_lu: e.loudness_range().ok()?,
        true_peak_dbtp: e.true_peak(0).ok()?,
        sample_peak_dbfs: e
            .sample_peak(0)
            .ok()
            .map(|p| 20.0 * p.max(1e-12).log10())
            .unwrap_or(f64::NEG_INFINITY),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sr: u32, hz: f64, secs: f64) -> Vec<f32> {
        let n = (sr as f64 * secs) as usize;
        (0..n)
            .map(|i| (std::f64::consts::TAU * hz * i as f64 / sr as f64).sin() as f32 * 0.8)
            .collect()
    }

    #[test]
    fn spectrogram_finds_tone() {
        let x = sine(8000, 1000.0, 1.0);
        let cfg = SpectralConfig::default();
        let frames = spectrogram(&x, &cfg, 256);
        assert!(!frames.is_empty());
        // 1024 点 FFT @8kHz：bin 128 = 1kHz，能量应显著
        let mid = &frames[frames.len() / 2];
        let avg: f32 = mid.db.iter().sum::<f32>() / mid.db.len() as f32;
        assert!(mid.db[128] > avg + 20.0, "bin128={} avg={}", mid.db[128], avg);
    }

    #[test]
    fn constellation_produces_stars_and_hashes() {
        let x = sine(8000, 1000.0, 2.0);
        let r = constellation(&x, 8000, &ConstellationConfig::default());
        assert!(!r.stars.is_empty());
        assert!(!r.hashes.is_empty());
        // 星点应聚集在 bin 128 附近
        let near: usize = r.stars.iter().filter(|s| (s.bin as i32 - 128).abs() <= 3).count();
        assert!(near > r.stars.len() / 2, "near={near} total={}", r.stars.len());
    }

    #[test]
    fn stats_on_sine() {
        let x = sine(48000, 440.0, 2.0);
        let s = level_stats(&x, 48000, 1).unwrap();
        assert!(s.integrated_lufs > -30.0 && s.integrated_lufs < 0.0);
    }
}
