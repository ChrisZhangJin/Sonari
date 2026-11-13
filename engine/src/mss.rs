//! 早媒体检测指纹（rust_mss `emd-core` 的 0.3.0 `default` profile）。
//!
//! 移植自 `rust_mss/crates/emd-core`（params.rs `DEFAULT`、stft.rs、spectrum.rs、
//! peaks.rs、hashes.rs、cap.rs），用于「波形和星座图」视图：
//!
//! | 步骤 | 参数 |
//! |---|---|
//! | 重采样 | 单声道 8 kHz |
//! | STFT | n_fft 512，周期 Hann 窗 480（居中补零到 512），hop 80（10 ms），center=True 两侧补零 256；f64 窗 + f64 FFT（推荐配置；`fft-f32` 为实验特性，不用） |
//! | 频带 | 保留 bin [4, 144) = 62.5–2250 Hz（格宽 15.625 Hz） |
//! | 找峰（fast peaks） | 功率谱 `P = re² + im²`（转 f32），跳过 sqrt / log10；最大值滤波 6（频）× 7（时），scipy 居中 / reflect 边界；`filtered == P && P > power_min`（= `10*log10|X| > -6.25 dB` 换算到功率，f32 位 0x3d6655c4） |
//! | 配对 | 锚点 t → 目标 t+1..=t+20 帧，目标 bin（频带内下标）∈ 1..=90 |
//! | 哈希上限 | 每秒 1500 个，20 帧一块按峰强排序，取满足预算的最大每块名次 K |
//!
//! 只做可视化：不算 md5（哈希 = `f1:f2:dt` 三元组本身）。

use realfft::RealFftPlanner;

use crate::decode::AudioBuffer;

pub const SAMPLE_RATE: u32 = 8000;
pub const N_FFT: usize = 512;
pub const WIN_LENGTH: usize = 480;
pub const HOP: usize = 80;
pub const BIN_LO: usize = 4;
pub const BIN_HI: usize = 144;
const N_BINS: usize = BIN_HI - BIN_LO;
const FREQ_SIZE: usize = 6;
const TIME_SIZE: usize = 7;
/// `-6.25 dB` 阈值换算到功率（emd-core `DEFAULT.power_min`）
const POWER_MIN: f32 = f32::from_bits(0x3d66_55c4);
pub const ZONE: usize = 20;
const F2_MIN: usize = 1;
const F2_MAX: usize = 90;
const CAP_PER_SEC: u64 = 1500;
const CAP_BLOCK: usize = 20;
const FRAMES_PER_SEC: u64 = 100;

/// 峰：`f` = 频带内下标（0..140），`t` = 帧，`power` = 峰强（`|X|²`）
#[derive(Debug, Clone, Copy)]
pub struct Peak {
    pub t: u32,
    pub f: u16,
    pub power: f32,
}

impl Peak {
    /// 峰强 dB（= `10*log10|X|`，与精确路径的 dB 同尺度）
    pub fn db(&self) -> f32 {
        5.0 * self.power.max(1e-20).log10()
    }
}

/// 哈希：锚点 (t, f1) → 目标 (t+dt, f2)；f1/f2 均为频带内下标
#[derive(Debug, Clone, Copy)]
pub struct Hash {
    pub t: u32,
    pub f1: u16,
    pub f2: u16,
    pub dt: u8,
}

pub struct Fingerprint {
    /// 帧数（帧 t 的中心 = t*HOP / 8000 秒）
    pub frames: usize,
    pub peaks: Vec<Peak>,
    pub hashes: Vec<Hash>,
    /// 上限前的峰数 / 哈希数（没触发上限时与结果相同）
    pub peaks_uncapped: usize,
    pub hashes_uncapped: usize,
}

/// 混合单声道 + 重采样到 8 kHz
pub fn to_8k_mono(buf: &AudioBuffer) -> Vec<f32> {
    let mono = AudioBuffer {
        sample_rate: buf.sample_rate,
        channels: 1,
        samples: buf.to_mono(),
    };
    if mono.sample_rate == SAMPLE_RATE {
        mono.samples
    } else {
        crate::transform::resample(&mono, SAMPLE_RATE).samples
    }
}

/// 8 kHz 单声道样本 → 指纹（含哈希上限）
pub fn fingerprint(y: &[f32]) -> Fingerprint {
    let (frames, power) = power_spectrum(y);
    let peaks = find_peaks(frames, &power);
    let all = count_hashes(&peaks, frames, |_| true);
    let budget = CAP_PER_SEC * frames as u64 / FRAMES_PER_SEC;
    let kept = if all as u64 > budget { thin(&peaks, frames, budget) } else { peaks.clone() };
    let hashes = gen_hashes(&kept, frames);
    Fingerprint {
        frames,
        peaks_uncapped: peaks.len(),
        hashes_uncapped: all,
        peaks: kept,
        hashes,
    }
}

/// 周期 Hann（scipy：`0.5 + 0.5*cos(linspace(-pi, pi, win+1)[:-1])`，f64），居中放进 n_fft
fn window() -> Vec<f64> {
    let mut w = vec![0.0f64; N_FFT];
    let off = (N_FFT - WIN_LENGTH) / 2;
    let step = 2.0 * std::f64::consts::PI / WIN_LENGTH as f64;
    for n in 0..WIN_LENGTH {
        let fac = n as f64 * step - std::f64::consts::PI;
        w[off + n] = 0.5 + 0.5 * fac.cos();
    }
    w
}

/// STFT（center=True，两侧补零 n_fft/2；f64 窗 + f64 FFT）→ 频带内功率 `re²+im²`
/// （帧 × N_BINS，行优先，转 f32）
fn power_spectrum(y: &[f32]) -> (usize, Vec<f32>) {
    let frames = 1 + y.len() / HOP;
    let pad = N_FFT / 2;
    let win = window();
    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(N_FFT);
    let mut inp = fft.make_input_vec();
    let mut out = fft.make_output_vec();
    let mut pw = vec![0.0f32; frames * N_BINS];
    for t in 0..frames {
        let start = (t * HOP) as isize - pad as isize;
        for (k, v) in inp.iter_mut().enumerate() {
            let i = start + k as isize;
            let s = if i >= 0 && (i as usize) < y.len() { y[i as usize] as f64 } else { 0.0 };
            *v = s * win[k];
        }
        fft.process(&mut inp, &mut out).expect("fft");
        let row = &mut pw[t * N_BINS..(t + 1) * N_BINS];
        for (b, v) in row.iter_mut().enumerate() {
            let c = out[BIN_LO + b];
            *v = (c.re * c.re + c.im * c.im) as f32;
        }
    }
    (frames, pw)
}

/// scipy `mode='reflect'`：边缘样本重复（d c b a | a b c d | d c b a）
#[inline]
fn reflect(i: isize, n: usize) -> usize {
    let n = n as isize;
    let p = 2 * n;
    let mut i = i.rem_euclid(p);
    if i >= n {
        i = p - 1 - i;
    }
    i as usize
}

/// 最大值滤波（6 频 × 7 时，可分离）+ 精确相等 + 功率阈值；输出按 (t, f) 升序。
/// 最大值滤波只依赖大小顺序，所以在功率谱上找峰与在 dB 上找峰等价（除 f32 舍入并列）
fn find_peaks(frames: usize, db: &[f32]) -> Vec<Peak> {
    let (t_lo, t_hi) = (-((TIME_SIZE / 2) as isize), (TIME_SIZE - 1 - TIME_SIZE / 2) as isize);
    let (f_lo, f_hi) = (-((FREQ_SIZE / 2) as isize), (FREQ_SIZE - 1 - FREQ_SIZE / 2) as isize);
    // 沿时间
    let mut tmax = vec![f32::NEG_INFINITY; db.len()];
    for t in 0..frames {
        for f in 0..N_BINS {
            let mut m = f32::NEG_INFINITY;
            for dt in t_lo..=t_hi {
                let tt = reflect(t as isize + dt, frames);
                m = m.max(db[tt * N_BINS + f]);
            }
            tmax[t * N_BINS + f] = m;
        }
    }
    // 沿频率 + 判峰
    let mut peaks = Vec::new();
    for t in 0..frames {
        let row = &tmax[t * N_BINS..(t + 1) * N_BINS];
        for f in 0..N_BINS {
            let v = db[t * N_BINS + f];
            if v <= POWER_MIN {
                continue;
            }
            let mut m = f32::NEG_INFINITY;
            for df in f_lo..=f_hi {
                m = m.max(row[reflect(f as isize + df, N_BINS)]);
            }
            if m == v {
                peaks.push(Peak { t: t as u32, f: f as u16, power: v });
            }
        }
    }
    peaks
}

/// 帧 → 峰切片的起点表（peaks 已按 t 排序）
fn frame_index(peaks: &[Peak], frames: usize) -> Vec<usize> {
    let mut start = vec![0usize; frames + 1];
    for p in peaks {
        start[p.t as usize + 1] += 1;
    }
    for t in 0..frames {
        start[t + 1] += start[t];
    }
    start
}

fn gen_hashes(peaks: &[Peak], frames: usize) -> Vec<Hash> {
    let start = frame_index(peaks, frames);
    let mut out = Vec::new();
    for i in 0..frames {
        for a in &peaks[start[i]..start[i + 1]] {
            for n in 1..=ZONE {
                let j = i + n;
                if j >= frames {
                    break;
                }
                for b in &peaks[start[j]..start[j + 1]] {
                    if (F2_MIN..=F2_MAX).contains(&(b.f as usize)) {
                        out.push(Hash { t: i as u32, f1: a.f, f2: b.f, dt: n as u8 });
                    }
                }
            }
        }
    }
    out
}

/// 只保留 `keep(i)` 的峰时会生成的哈希数
fn count_hashes(peaks: &[Peak], frames: usize, keep: impl Fn(usize) -> bool) -> usize {
    // 每帧：保留的锚点数、保留且 f 在目标范围内的峰数
    let mut anchors = vec![0usize; frames];
    let mut targets = vec![0usize; frames + 1];
    for (i, p) in peaks.iter().enumerate() {
        if keep(i) {
            anchors[p.t as usize] += 1;
            if (F2_MIN..=F2_MAX).contains(&(p.f as usize)) {
                targets[p.t as usize + 1] += 1;
            }
        }
    }
    for t in 0..frames {
        targets[t + 1] += targets[t]; // 前缀和：targets[t] = 帧 < t 的目标数
    }
    let mut n = 0;
    for (i, &a) in anchors.iter().enumerate() {
        if a > 0 {
            let lo = (i + 1).min(frames);
            let hi = (i + ZONE + 1).min(frames);
            n += a * (targets[hi] - targets[lo]);
        }
    }
    n
}

/// 哈希上限：块内按峰强排名（同强度按生成顺序），二分取预算内最大的名次上限 K
fn thin(peaks: &[Peak], frames: usize, budget: u64) -> Vec<Peak> {
    let mut rank = vec![0u32; peaks.len()];
    let mut i = 0;
    while i < peaks.len() {
        let blk = peaks[i].t as usize / CAP_BLOCK;
        let mut j = i;
        while j < peaks.len() && peaks[j].t as usize / CAP_BLOCK == blk {
            j += 1;
        }
        let mut idx: Vec<usize> = (i..j).collect();
        idx.sort_by(|&a, &b| peaks[b].power.total_cmp(&peaks[a].power).then(a.cmp(&b)));
        for (r, &k) in idx.iter().enumerate() {
            rank[k] = r as u32;
        }
        i = j;
    }
    let max_rank = rank.iter().copied().max().unwrap_or(0) + 1;
    let count = |k: u32| count_hashes(peaks, frames, |i| rank[i] < k);
    // 最大的 K（>=1）使哈希数 <= 预算
    let (mut lo, mut hi) = (1u32, max_rank);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if count(mid) as u64 <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    // 一个哈希都不剩时往上抬，直到有哈希
    let mut k = lo;
    while k < max_rank && count(k) == 0 {
        k += 1;
    }
    peaks.iter().enumerate().filter(|(i, _)| rank[*i] < k).map(|(_, p)| *p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tones(secs: f32) -> Vec<f32> {
        let n = (SAMPLE_RATE as f32 * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                // 每 0.25 s 换一个频率的音
                let f = [440.0, 880.0, 1320.0, 660.0][(t / 0.25) as usize % 4];
                (2.0 * std::f32::consts::PI * f * t).sin() * 0.5
            })
            .collect()
    }

    #[test]
    fn power_min_is_minus_6_25_db() {
        // 10*log10(sqrt(P)) = 5*log10(P)
        assert!((5.0 * (POWER_MIN as f64).log10() + 6.25).abs() < 1e-5);
    }

    #[test]
    fn reflect_matches_scipy() {
        // n=4: ... 1 0 | 0 1 2 3 | 3 2 ...
        assert_eq!(reflect(-1, 4), 0);
        assert_eq!(reflect(-2, 4), 1);
        assert_eq!(reflect(4, 4), 3);
        assert_eq!(reflect(5, 4), 2);
    }

    #[test]
    fn tone_peaks_land_on_their_bins() {
        let fp = fingerprint(&tones(1.0));
        assert_eq!(fp.frames, 1 + 8000 / HOP);
        assert!(!fp.peaks.is_empty());
        // 440 Hz → 绝对 bin 28.16 → 频带内 24；中段帧里应有它的峰
        let hz = |f: u16| (f as f32 + BIN_LO as f32) * SAMPLE_RATE as f32 / N_FFT as f32;
        assert!(fp.peaks.iter().any(|p| p.t > 5 && p.t < 20 && (hz(p.f) - 440.0).abs() < 20.0));
        // 有序 (t, f)
        assert!(fp.peaks.windows(2).all(|w| (w[0].t, w[0].f) < (w[1].t, w[1].f)));
        // 哈希都在 zone / 目标范围内
        assert!(fp.hashes.iter().all(|h| (1..=ZONE as u8).contains(&h.dt) && (1..=90).contains(&h.f2)));
    }

    #[test]
    fn cap_limits_hash_rate() {
        // 白噪声：峰多 → 触发上限
        let mut s = 0x1234_5678u32;
        let y: Vec<f32> = (0..16000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32 - 0.5) * 0.8
            })
            .collect();
        let fp = fingerprint(&y);
        let budget = CAP_PER_SEC * fp.frames as u64 / FRAMES_PER_SEC;
        assert!(fp.hashes_uncapped as u64 > budget, "噪声应超出预算");
        assert!(fp.hashes.len() as u64 <= budget);
        assert!(!fp.hashes.is_empty());
        assert_eq!(count_hashes(&fp.peaks, fp.frames, |_| true), fp.hashes.len());
    }
}
