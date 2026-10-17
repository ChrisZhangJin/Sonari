//! DSP 基础 —— 对标 `libdspb.dll`。
//!
//! 覆盖 libdspb 里与频谱/星座图直接相关的部分：
//! - 窗函数（`DSPB_CreateWindow/ApplyWindow`）：Hann/Hamming/Blackman/BlackmanHarris
//! - 实数 FFT（`DSPB_FFTProc*`，ocenaudio 用 PFFFT/Kiss，这里用 realfft）
//! - PSD→dB（`DSPB_PSDtoDB` + 动态范围/归一化，对应 libocen.spectral.custom.* 配置）
//! - Bark/Mel 标尺（`DSPBUTIL_Freq2Bark/Freq2Mels`）——频谱图非线性纵轴用

use realfft::RealFftPlanner;
use realfft::RealToComplex;

/// 窗函数类型，对应 ocenaudio 的 `libocen.spectral.custom.wintype`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowType {
    Hann,
    Hamming,
    Blackman,
    BlackmanHarris,
    Rectangular,
}

impl WindowType {
    pub fn from_config(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "hann" => Self::Hann,
            "hamming" => Self::Hamming,
            "blackman" => Self::Blackman,
            "blackman-harris" | "blackmanharris" => Self::BlackmanHarris,
            "rect" | "rectangular" | "none" => Self::Rectangular,
            _ => return None,
        })
    }

    pub fn generate(&self, n: usize) -> Vec<f32> {
        let mut w = vec![1.0f32; n];
        if n <= 1 {
            return w;
        }
        let nm1 = (n - 1) as f64;
        for (i, v) in w.iter_mut().enumerate() {
            let x = i as f64 / nm1;
            *v = match self {
                Self::Rectangular => 1.0,
                Self::Hann => 0.5 - 0.5 * (std::f64::consts::TAU * x).cos(),
                Self::Hamming => 0.54 - 0.46 * (std::f64::consts::TAU * x).cos(),
                Self::Blackman => 0.42 - 0.5 * (std::f64::consts::TAU * x).cos()
                    + 0.08 * (2.0 * std::f64::consts::TAU * x).cos(),
                Self::BlackmanHarris => 0.35875 - 0.48829 * (std::f64::consts::TAU * x).cos()
                    + 0.14128 * (2.0 * std::f64::consts::TAU * x).cos()
                    - 0.01168 * (3.0 * std::f64::consts::TAU * x).cos(),
            } as f32;
        }
        w
    }
}

/// 单例 FFT 处理器缓存（按 FFT 长度复用，避免反复建 plan）。
pub struct FftCache {
    planner: RealFftPlanner<f32>,
    cached: Option<(usize, std::sync::Arc<dyn RealToComplex<f32>>)>,
    input: Vec<f32>,
    spectrum: Vec<num_complex::Complex<f32>>,
    scratch: Vec<num_complex::Complex<f32>>,
    out: Vec<f32>,
}

impl FftCache {
    pub fn new() -> Self {
        Self {
            planner: RealFftPlanner::new(),
            cached: None,
            input: Vec::new(),
            spectrum: Vec::new(),
            scratch: Vec::new(),
            out: Vec::new(),
        }
    }

    fn plan(&mut self, n: usize) -> std::sync::Arc<dyn RealToComplex<f32>> {
        if self.cached.as_ref().map(|(k, _)| *k) != Some(n) {
            let p = self.planner.plan_fft_forward(n);
            self.input = p.make_input_vec();
            self.spectrum = p.make_output_vec();
            self.scratch = p.make_scratch_vec();
            self.out = vec![0.0; p.complex_len()];
            self.cached = Some((n, p));
        }
        self.cached.as_ref().unwrap().1.clone()
    }

    /// 就地 FFT：输入 n 个实样本（已加窗），输出 n/2+1 个 bin 的幅度谱。
    pub fn magnitude(&mut self, windowed: &[f32]) -> Vec<f32> {
        let n = windowed.len();
        let plan = self.plan(n);
        self.input[..n].copy_from_slice(windowed);
        for v in self.spectrum.iter_mut() {
            *v = num_complex::Complex::new(0.0, 0.0);
        }
        let _ = plan.process_with_scratch(
            &mut self.input,
            &mut self.spectrum,
            &mut self.scratch,
        );
        for (i, c) in self.spectrum.iter().enumerate() {
            self.out[i] = (c.re * c.re + c.im * c.im).sqrt() / (n as f32 / 2.0);
        }
        self.out.clone()
    }
}

/// PSD → dB，含归一化与动态范围裁剪。
/// 对应 ocenaudio 的 `PSDtoDB` + `spectral.custom.{normalize,dynrange}`。
pub fn psd_to_db(mag: &[f32], normalize: bool, dyn_range_db: f32) -> Vec<f32> {
    let peak = if normalize {
        mag.iter().cloned().fold(0.0f32, f32::max).max(1e-12)
    } else {
        1.0
    };
    mag.iter()
        .map(|&m| {
            let v = m / peak;
            let db = 20.0 * (v.max(1e-12).log10());
            // 参考峰值 0dB，向下裁剪 dyn_range
            db.max(-dyn_range_db)
        })
        .collect()
}

/// 频率 → Bark（`DSPBUTIL_Freq2Bark`，Zwicker 公式）。
pub fn freq_to_bark(hz: f64) -> f64 {
    13.0 * (0.00076 * hz).atan() + 3.5 * ((hz / 7500.0).powi(2)).atan()
}

/// 频率 → Mel（`DSPBUTIL_Freq2Mels`）。
pub fn freq_to_mel(hz: f64) -> f64 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

pub fn mel_to_freq(mel: f64) -> f64 {
    700.0 * (10f64.powf(mel / 2595.0) - 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_sums() {
        let w = WindowType::Hann.generate(1024);
        let mean: f32 = w.iter().sum::<f32>() / w.len() as f32;
        assert!((mean - 0.5).abs() < 0.01, "hann mean {mean}");
    }

    #[test]
    fn fft_sine_tone() {
        let n = 1024usize;
        let sr = 8000.0f32;
        // 1kHz 正弦 → bin 128
        let freq_bin = 128usize;
        let sig: Vec<f32> = (0..n)
            .map(|i| (std::f64::consts::TAU * freq_bin as f64 * i as f64 / n as f64).sin() as f32)
            .collect();
        let mut fft = FftCache::new();
        let mag = fft.magnitude(&sig);
        let peak = mag
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(peak.0, freq_bin);
        assert!((peak.1 - 1.0).abs() < 0.05, "amp {}", peak.1);
        let _ = sr;
    }

    #[test]
    fn bark_and_mel() {
        assert!((freq_to_bark(1000.0) - 8.9).abs() < 0.6);
        assert!((freq_to_mel(1000.0) - 1000.0).abs() < 50.0);
        let back = mel_to_freq(freq_to_mel(440.0));
        assert!((back - 440.0).abs() < 1e-6);
    }
}
