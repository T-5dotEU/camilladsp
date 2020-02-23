// Based on https://github.com/korken89/biquad-rs
// coeffs: https://arachnoid.com/BiQuadDesigner/index.html

//mod filters;

use crate::filters::Filter;
use config;

// Sample format
//type SmpFmt = i16;
use PrcFmt;
use Res;

/// Struct to hold the biquad coefficients
#[derive(Clone, Copy, Debug)]
pub struct BiquadCoefficients {
    pub a1: PrcFmt,
    pub a2: PrcFmt,
    pub b0: PrcFmt,
    pub b1: PrcFmt,
    pub b2: PrcFmt,
}

impl BiquadCoefficients {
    pub fn new(a1: PrcFmt, a2: PrcFmt, b0: PrcFmt, b1: PrcFmt, b2: PrcFmt) -> Self {
        BiquadCoefficients { a1, a2, b0, b1, b2 }
    }

    pub fn normalize(
        a0: PrcFmt,
        a1: PrcFmt,
        a2: PrcFmt,
        b0: PrcFmt,
        b1: PrcFmt,
        b2: PrcFmt,
    ) -> Self {
        let a1n = a1 / a0;
        let a2n = a2 / a0;
        let b0n = b0 / a0;
        let b1n = b1 / a0;
        let b2n = b2 / a0;
        // eprintln!("a1={}\na2={}\nb0={}\nb1={}\nb2={}", a1n, a2n, b0n, b1n, b2n);
        BiquadCoefficients {
            a1: a1n,
            a2: a2n,
            b0: b0n,
            b1: b1n,
            b2: b2n,
        }
    }

    /// Create biquad filters from config.
    /// Filter types
    /// - Free: just coefficients
    /// - Highpass: second order highpass specified by frequency and Q-value.
    /// - Lowpass: second order lowpass specified by frequency and Q-value.
    /// - Peaking: parametric peaking filter specified by gain, frequency and Q-value.
    /// - Highshelf: shelving filter affecting high frequencies with arbitrary slope in between.
    ///   The frequency specified is the middle of the slope
    /// - Lowshelf: shelving filter affecting low frequencies with arbitrary slope in between.
    ///   The frequency specified is the middle of the slope
    pub fn from_config(fs: usize, parameters: config::BiquadParameters) -> Self {
        match parameters {
            config::BiquadParameters::Free { a1, a2, b0, b1, b2 } => {
                BiquadCoefficients::new(a1, a2, b0, b1, b2)
            }
            config::BiquadParameters::Highpass { freq, q } => {
                let omega = 2.0 * (std::f64::consts::PI as PrcFmt) * freq / (fs as PrcFmt);
                let sn = omega.sin();
                let cs = omega.cos();
                let alpha = sn / (2.0 * q);
                let b0 = (1.0 + cs) / 2.0;
                let b1 = -(1.0 + cs);
                let b2 = (1.0 + cs) / 2.0;
                let a0 = 1.0 + alpha;
                let a1 = -2.0 * cs;
                let a2 = 1.0 - alpha;
                BiquadCoefficients::normalize(a0, a1, a2, b0, b1, b2)
            }
            config::BiquadParameters::Lowpass { freq, q } => {
                let omega = 2.0 * (std::f64::consts::PI as PrcFmt) * freq / (fs as PrcFmt);
                let sn = omega.sin();
                let cs = omega.cos();
                let alpha = sn / (2.0 * q);
                let b0 = (1.0 - cs) / 2.0;
                let b1 = 1.0 - cs;
                let b2 = (1.0 - cs) / 2.0;
                let a0 = 1.0 + alpha;
                let a1 = -2.0 * cs;
                let a2 = 1.0 - alpha;
                BiquadCoefficients::normalize(a0, a1, a2, b0, b1, b2)
            }
            config::BiquadParameters::Peaking { freq, gain, q } => {
                let omega = 2.0 * (std::f64::consts::PI as PrcFmt) * freq / (fs as PrcFmt);
                let sn = omega.sin();
                let cs = omega.cos();
                let ampl = (10.0 as PrcFmt).powf(gain / 40.0);
                let alpha = sn / (2.0 * q);
                let b0 = 1.0 + (alpha * ampl);
                let b1 = -2.0 * cs;
                let b2 = 1.0 - (alpha * ampl);
                let a0 = 1.0 + (alpha / ampl);
                let a1 = -2.0 * cs;
                let a2 = 1.0 - (alpha / ampl);
                BiquadCoefficients::normalize(a0, a1, a2, b0, b1, b2)
            }

            config::BiquadParameters::Highshelf { freq, slope, gain } => {
                let omega = 2.0 * (std::f64::consts::PI as PrcFmt) * freq / (fs as PrcFmt);
                let sn = omega.sin();
                let cs = omega.cos();
                let ampl = (10.0 as PrcFmt).powf(gain / 40.0);
                let alpha =
                    sn / 2.0 * ((ampl + 1.0 / ampl) * (1.0 / (slope / 12.0) - 1.0) + 2.0).sqrt();
                let beta = 2.0 * ampl.sqrt() * alpha;
                let b0 = ampl * ((ampl + 1.0) + (ampl - 1.0) * cs + beta);
                let b1 = -2.0 * ampl * ((ampl - 1.0) + (ampl + 1.0) * cs);
                let b2 = ampl * ((ampl + 1.0) + (ampl - 1.0) * cs - beta);
                let a0 = (ampl + 1.0) - (ampl - 1.0) * cs + beta;
                let a1 = 2.0 * ((ampl - 1.0) - (ampl + 1.0) * cs);
                let a2 = (ampl + 1.0) - (ampl - 1.0) * cs - beta;
                BiquadCoefficients::normalize(a0, a1, a2, b0, b1, b2)
            }
            config::BiquadParameters::Lowshelf { freq, slope, gain } => {
                let omega = 2.0 * (std::f64::consts::PI as PrcFmt) * freq / (fs as PrcFmt);
                let sn = omega.sin();
                let cs = omega.cos();
                let ampl = (10.0 as PrcFmt).powf(gain / 40.0);
                let alpha =
                    sn / 2.0 * ((ampl + 1.0 / ampl) * (1.0 / (slope / 12.0) - 1.0) + 2.0).sqrt();
                let beta = 2.0 * ampl.sqrt() * alpha;
                let b0 = ampl * ((ampl + 1.0) - (ampl - 1.0) * cs + beta);
                let b1 = 2.0 * ampl * ((ampl - 1.0) - (ampl + 1.0) * cs);
                let b2 = ampl * ((ampl + 1.0) - (ampl - 1.0) * cs - beta);
                let a0 = (ampl + 1.0) + (ampl - 1.0) * cs + beta;
                let a1 = -2.0 * ((ampl - 1.0) + (ampl + 1.0) * cs);
                let a2 = (ampl + 1.0) + (ampl - 1.0) * cs - beta;
                BiquadCoefficients::normalize(a0, a1, a2, b0, b1, b2)
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Biquad {
    pub s1: PrcFmt,
    pub s2: PrcFmt,
    coeffs: BiquadCoefficients,
}

impl Biquad {
    /// Creates a Direct Form 2 Transposed biquad filter from a set of coefficients
    pub fn new(coefficients: BiquadCoefficients) -> Self {
        Biquad {
            s1: 0.0,
            s2: 0.0,
            coeffs: coefficients,
        }
    }

    /// Process a single sample
    fn process_single(&mut self, input: PrcFmt) -> PrcFmt {
        let out = self.s1 + self.coeffs.b0 * input;
        self.s1 = self.s2 + self.coeffs.b1 * input - self.coeffs.a1 * out;
        self.s2 = self.coeffs.b2 * input - self.coeffs.a2 * out;
        out
    }
}

impl Filter for Biquad {
    fn process_waveform(&mut self, waveform: &mut Vec<PrcFmt>) -> Res<()> {
        for item in waveform.iter_mut() {
            *item = self.process_single(*item);
        }
        //let out = input.iter().map(|s| self.process_single(*s)).collect::<Vec<PrcFmt>>();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::PrcFmt;
    use biquad::{Biquad, BiquadCoefficients};
    use config::BiquadParameters;
    use filters::Filter;
    use rustfft::num_complex::Complex;

    fn is_close(left: PrcFmt, right: PrcFmt, maxdiff: PrcFmt) -> bool {
        println!("{} - {}", left, right);
        (left - right).abs() < maxdiff
    }

    fn compare_waveforms(left: Vec<PrcFmt>, right: Vec<PrcFmt>, maxdiff: PrcFmt) -> bool {
        for (val_l, val_r) in left.iter().zip(right.iter()) {
            if !is_close(*val_l, *val_r, maxdiff) {
                return false;
            }
        }
        true
    }

    fn gain_and_phase(coeffs: BiquadCoefficients, f: PrcFmt, fs: usize) -> (PrcFmt, PrcFmt) {
        let pi = std::f64::consts::PI as PrcFmt;
        let z = (Complex::i() * 2.0 * pi * f / (fs as PrcFmt)).exp();
        let a = (coeffs.b0 + coeffs.b1 * z.powi(-1) + coeffs.b2 * z.powi(-2))
            / (1.0 + coeffs.a1 * z.powi(-1) + coeffs.a2 * z.powi(-2));
        let (magn, ang) = a.to_polar();
        let gain = 20.0 * magn.log10();
        let phase = 180.0 / pi * ang;
        (gain, phase)
    }

    #[test]
    fn check_result() {
        let conf = BiquadParameters::Lowpass {
            freq: 10000.0,
            q: 0.5,
        };
        let coeffs = BiquadCoefficients::from_config(44100, conf);
        let mut wave = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let expected = vec![0.215, 0.461, 0.281, 0.039, 0.004, 0.0, 0.0, 0.0];
        let mut filter = Biquad::new(coeffs);
        filter.process_waveform(&mut wave).unwrap();
        assert!(compare_waveforms(wave, expected, 1e-3));
    }

    #[test]
    fn make_lowpass() {
        let conf = BiquadParameters::Lowpass {
            freq: 100.0,
            q: 0.707,
        };
        let coeffs = BiquadCoefficients::from_config(44100, conf);
        let (gain_f0, _) = gain_and_phase(coeffs, 100.0, 44100);
        let (gain_hf, _) = gain_and_phase(coeffs, 400.0, 44100);
        let (gain_lf, _) = gain_and_phase(coeffs, 10.0, 44100);
        assert!(is_close(gain_f0, -3.0, 0.1));
        assert!(is_close(gain_lf, 0.0, 0.1));
        assert!(is_close(gain_hf, -24.0, 0.2));
    }

    #[test]
    fn make_highpass() {
        let conf = BiquadParameters::Highpass {
            freq: 100.0,
            q: 0.707,
        };
        let coeffs = BiquadCoefficients::from_config(44100, conf);
        let (gain_f0, _) = gain_and_phase(coeffs, 100.0, 44100);
        let (gain_hf, _) = gain_and_phase(coeffs, 400.0, 44100);
        let (gain_lf, _) = gain_and_phase(coeffs, 25.0, 44100);
        assert!(is_close(gain_f0, -3.0, 0.1));
        assert!(is_close(gain_lf, -24.0, 0.2));
        assert!(is_close(gain_hf, 0.0, 0.1));
    }

    #[test]
    fn make_peaking() {
        let conf = BiquadParameters::Peaking {
            freq: 100.0,
            gain: 7.0,
            q: 3.0,
        };
        let coeffs = BiquadCoefficients::from_config(44100, conf);
        let (gain_f0, _) = gain_and_phase(coeffs, 100.0, 44100);
        let (gain_hf, _) = gain_and_phase(coeffs, 400.0, 44100);
        let (gain_lf, _) = gain_and_phase(coeffs, 25.0, 44100);
        assert!(is_close(gain_f0, 7.0, 0.1));
        assert!(is_close(gain_lf, 0.0, 0.1));
        assert!(is_close(gain_hf, 0.0, 0.1));
    }

    #[test]
    fn make_highshelf() {
        let conf = BiquadParameters::Highshelf {
            freq: 100.0,
            slope: 6.0,
            gain: -24.0,
        };
        let coeffs = BiquadCoefficients::from_config(44100, conf);
        let (gain_f0, _) = gain_and_phase(coeffs, 100.0, 44100);
        let (gain_f0h, _) = gain_and_phase(coeffs, 200.0, 44100);
        let (gain_f0l, _) = gain_and_phase(coeffs, 50.0, 44100);
        let (gain_hf, _) = gain_and_phase(coeffs, 10000.0, 44100);
        let (gain_lf, _) = gain_and_phase(coeffs, 1.0, 44100);
        assert!(is_close(gain_f0, -12.0, 0.1));
        assert!(is_close(gain_f0h, -18.0, 1.0));
        assert!(is_close(gain_f0l, -6.0, 1.0));
        assert!(is_close(gain_lf, 0.0, 0.1));
        assert!(is_close(gain_hf, -24.0, 0.1));
    }

    #[test]
    fn make_lowshelf() {
        let conf = BiquadParameters::Lowshelf {
            freq: 100.0,
            slope: 6.0,
            gain: -24.0,
        };
        let coeffs = BiquadCoefficients::from_config(44100, conf);
        let (gain_f0, _) = gain_and_phase(coeffs, 100.0, 44100);
        let (gain_f0h, _) = gain_and_phase(coeffs, 200.0, 44100);
        let (gain_f0l, _) = gain_and_phase(coeffs, 50.0, 44100);
        let (gain_hf, _) = gain_and_phase(coeffs, 10000.0, 44100);
        let (gain_lf, _) = gain_and_phase(coeffs, 1.0, 44100);
        assert!(is_close(gain_f0, -12.0, 0.1));
        assert!(is_close(gain_f0h, -6.0, 1.0));
        assert!(is_close(gain_f0l, -18.0, 1.0));
        assert!(is_close(gain_lf, -24.0, 0.1));
        assert!(is_close(gain_hf, -0.0, 0.1));
    }
}
