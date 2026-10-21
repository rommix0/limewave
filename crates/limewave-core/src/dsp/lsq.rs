//! Least-squares fits.

use std::f64::consts::TAU;
use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use super::spectrum::Complex64;

/// Added to the diagonal of the normal equations so near-duplicate frequencies stay solvable.
const RIDGE: f64 = 1e-9;

/// The complex amplitude `c` of each frequency in `freqs_hz` that best explains `x`, so that
/// `x[t] ≈ Re Σ c·e^{iωt}` with `ω = 2π·f / rate` and `t = 0` at `x[0]`. In other words
/// `|c|` is the amplitude and `arg c` the phase at `x[0]` of `|c|·cos(ωt + arg c)`.
///
/// All frequencies are fitted jointly, as cosine and sine pairs with constant amplitude over
/// `x`, by solving the normal equations with a Cholesky factorization and a 1e-9 ridge.
/// Returns all zeros (one per frequency) when the system is singular, `x` is empty or `rate`
/// is 0. A frequency at 0 Hz or the Nyquist frequency has no sine part, so its `c` is real.
pub fn harmonic_fit(x: &[f32], freqs_hz: &[f64], rate: u32) -> Vec<Complex64> {
    let k = freqs_hz.len();
    let zeros = vec![Complex64::default(); k];
    if x.is_empty() || rate == 0 || k == 0 {
        return zeros;
    }
    let omegas: Vec<f64> = freqs_hz.iter().map(|f| TAU * f / f64::from(rate)).collect();
    // Basis column j < k is cos(ω_j t); column k + j is −sin(ω_j t), so that the coefficients
    // (a, b) give a·cos − b·sin = Re((a + ib)·e^{iωt}).
    let m = 2 * k;
    let mut ata = vec![0.0f64; m * m];
    let mut atx = vec![0.0f64; m];
    let mut row = vec![0.0f64; m];
    for (t, &v) in x.iter().enumerate() {
        for (j, &w) in omegas.iter().enumerate() {
            let (s, c) = (w * t as f64).sin_cos();
            row[j] = c;
            row[k + j] = -s;
        }
        let v = f64::from(v);
        for i in 0..m {
            atx[i] += row[i] * v;
            // Upper triangle only; mirrored below.
            for j in i..m {
                ata[i * m + j] += row[i] * row[j];
            }
        }
    }
    for i in 0..m {
        ata[i * m + i] += RIDGE;
        for j in 0..i {
            ata[i * m + j] = ata[j * m + i];
        }
    }
    let Some(coef) = cholesky_solve(&mut ata, &atx, m) else {
        return zeros;
    };
    (0..k)
        .map(|j| Complex64::new(coef[j], coef[k + j]))
        .collect()
}

/// Solves `A·y = b` for a symmetric positive-definite `m × m` matrix `a` (row-major),
/// factorizing it in place. `None` when `a` is not positive definite or the result is not
/// finite.
fn cholesky_solve(a: &mut [f64], b: &[f64], m: usize) -> Option<Vec<f64>> {
    if !cholesky_factor(a, m) {
        return None;
    }
    cholesky_substitute(a, b, m)
}

/// Factorizes the symmetric positive-definite `m × m` matrix `a` (row-major) in place as
/// `A = L·Lᵀ`, with `L` in the lower triangle. `false` when `a` is not positive definite.
fn cholesky_factor(a: &mut [f64], m: usize) -> bool {
    for j in 0..m {
        let d = a[j * m + j] - (0..j).map(|p| a[j * m + p] * a[j * m + p]).sum::<f64>();
        if !d.is_finite() || d <= 0.0 {
            return false;
        }
        let d = d.sqrt();
        a[j * m + j] = d;
        for i in j + 1..m {
            let s = a[i * m + j] - (0..j).map(|p| a[i * m + p] * a[j * m + p]).sum::<f64>();
            a[i * m + j] = s / d;
        }
    }
    true
}

/// Solves `L·Lᵀ·y = b` for the factor from [`cholesky_factor`]: `L·z = b`, then `Lᵀ·y = z`.
/// `None` when the result is not finite.
fn cholesky_substitute(l: &[f64], b: &[f64], m: usize) -> Option<Vec<f64>> {
    let mut y = b.to_vec();
    for i in 0..m {
        let s: f64 = (0..i).map(|p| l[i * m + p] * y[p]).sum();
        y[i] = (y[i] - s) / l[i * m + i];
    }
    for i in (0..m).rev() {
        let s: f64 = (i + 1..m).map(|p| l[p * m + i] * y[p]).sum();
        y[i] = (y[i] - s) / l[i * m + i];
    }
    y.iter().all(|v| v.is_finite()).then_some(y)
}

/// The most harmonics one direct joint fit solves for (256 unknowns). With more, the band
/// solves precondition an iterative solution of the whole system.
const BAND_HARMONICS: usize = 128;

/// The conjugate-gradient iterations stop when the residual is this small relative to the
/// right-hand side, or after [`MAX_ITERATIONS`].
const TOLERANCE: f64 = 1e-12;
const MAX_ITERATIONS: usize = 200;

/// The same joint least-squares fit as [`harmonic_fit`], for many harmonics of one fundamental
/// over many windows of one length, at a fraction of the cost.
///
/// The frequencies are `h·f0` for whole harmonic numbers `h`, so every entry of the normal
/// matrix is `½(C[h − h'] ± C[h + h'])` or `−½(S[h + h'] ± S[h' − h])`, where `C[n]` and `S[n]`
/// are the sums of `cos(n·ω0·t)` and `sin(n·ω0·t)` over the window. Those `2·(max h) + 1` sums
/// are computed once.
///
/// - Up to 128 harmonics: the matrix is built from the sums and factorized once (with the same
///   1e-9 ridge), and each fit is one substitution, exactly as in [`harmonic_fit`].
/// - More: the harmonics are split into bands of at most 128 adjacent ones (in harmonic-number
///   order), and each band's own block of the matrix is factorized. The whole system is then
///   solved by conjugate gradients, preconditioned by those band solves, to a relative
///   residual of 1e-12. The matrix is never stored: it is a Toeplitz plus Hankel operator in
///   the harmonic number, applied with two FFTs of about `4·(max h)` points. Memory and time
///   grow about linearly with the number of harmonics, and the result is the joint fit, not an
///   approximation of it. (Fitting the bands separately would not do: on a window that is not
///   a whole number of periods, harmonics far apart still interact at the percent level.)
pub struct HarmonicFitter {
    len: usize,
    w0: f64,
    harmonics: Vec<usize>,
    /// The bands, in harmonic-number order; empty when the fitter cannot fit.
    bands: Vec<Band>,
    /// The whole matrix as an FFT operator, when there is more than one band.
    operator: Option<Operator>,
}

/// One band of adjacent harmonics.
struct Band {
    /// Indices into the fitter's harmonics, in harmonic-number order.
    members: Vec<usize>,
    /// The Cholesky factor of the band's block of the normal matrix; `None` when singular.
    factor: Option<Vec<f64>>,
}

/// The normal matrix as a circular convolution: with `p[h] = a[h]`, `p[−h] = a[h]` and
/// `q[h] = b[h]`, `q[−h] = −b[h]` for the cosine and sine coefficients `a` and `b` (indexed by
/// harmonic number, negative indices wrapped), the products are `½(C ⊛ p + S ⊛ q)` for the
/// cosine rows and `½(C ⊛ q − S ⊛ p)` for the sine rows, read at `0..=max h`.
struct Operator {
    size: usize,
    forward: Arc<dyn RealToComplex<f64>>,
    inverse: Arc<dyn ComplexToReal<f64>>,
    /// The spectra of the wrapped `C` (even) and `S` (odd) sums, scaled by `1 / (2·size)`.
    c_hat: Vec<Complex64>,
    s_hat: Vec<Complex64>,
}

impl Operator {
    /// The operator for the window sums `c[n]`, `s[n]`, `n = 0..=2·top`.
    fn new(c: &[f64], s: &[f64], top: usize) -> Operator {
        // Kernel indices run over −2·top..=2·top, and the vectors over −top..=top, so a
        // circle longer than 4·top keeps every product that is read free of wrap-around.
        let size = (4 * top + 2).next_power_of_two();
        let mut planner = RealFftPlanner::<f64>::new();
        let forward = planner.plan_fft_forward(size);
        let inverse = planner.plan_fft_inverse(size);
        let spectrum = |odd: bool, k: &[f64]| {
            let mut buf = forward.make_input_vec();
            buf[0] = k[0];
            for n in 1..=2 * top {
                buf[n] = k[n];
                buf[size - n] = if odd { -k[n] } else { k[n] };
            }
            let mut out = forward.make_output_vec();
            let _ = forward.process(&mut buf, &mut out);
            let scale = 0.5 / size as f64;
            out.iter().map(|v| v * scale).collect::<Vec<_>>()
        };
        Operator {
            size,
            c_hat: spectrum(false, c),
            s_hat: spectrum(true, s),
            forward,
            inverse,
        }
    }

    /// The normal matrix times the coefficients `y` (cosine parts then sine parts, in the
    /// order of `harmonics`), plus the ridge.
    fn apply(&self, harmonics: &[usize], y: &[f64]) -> Vec<f64> {
        let k = harmonics.len();
        let (mut p, mut q) = (self.forward.make_input_vec(), self.forward.make_input_vec());
        for (j, &h) in harmonics.iter().enumerate() {
            p[h] += y[j];
            p[(self.size - h) % self.size] += y[j];
            q[h] += y[k + j];
            q[(self.size - h) % self.size] -= y[k + j];
        }
        let (mut ph, mut qh) = (
            self.forward.make_output_vec(),
            self.forward.make_output_vec(),
        );
        let _ = self.forward.process(&mut p, &mut ph);
        let _ = self.forward.process(&mut q, &mut qh);
        let mut cos_rows: Vec<Complex64> = (0..ph.len())
            .map(|i| self.c_hat[i] * ph[i] + self.s_hat[i] * qh[i])
            .collect();
        let mut sin_rows: Vec<Complex64> = (0..ph.len())
            .map(|i| self.c_hat[i] * qh[i] - self.s_hat[i] * ph[i])
            .collect();
        let last = ph.len() - 1;
        for spec in [&mut cos_rows, &mut sin_rows] {
            spec[0].im = 0.0;
            spec[last].im = 0.0;
        }
        let (mut cos_out, mut sin_out) = (
            self.inverse.make_output_vec(),
            self.inverse.make_output_vec(),
        );
        let _ = self.inverse.process(&mut cos_rows, &mut cos_out);
        let _ = self.inverse.process(&mut sin_rows, &mut sin_out);
        let mut out = vec![0.0; 2 * k];
        for (j, &h) in harmonics.iter().enumerate() {
            out[j] = cos_out[h] + RIDGE * y[j];
            out[k + j] = sin_out[h] + RIDGE * y[k + j];
        }
        out
    }
}

impl HarmonicFitter {
    /// A fitter for windows of `len` samples and the harmonic numbers `harmonics` (each ≥ 1,
    /// no repeats) of `f0_hz`, at `rate`.
    pub fn new(len: usize, f0_hz: f64, harmonics: &[usize], rate: u32) -> HarmonicFitter {
        HarmonicFitter::with_band_size(len, f0_hz, harmonics, rate, BAND_HARMONICS)
    }

    /// [`HarmonicFitter::new`] with at most `band` harmonics in one band.
    fn with_band_size(
        len: usize,
        f0_hz: f64,
        harmonics: &[usize],
        rate: u32,
        band: usize,
    ) -> HarmonicFitter {
        let w0 = if rate == 0 {
            0.0
        } else {
            TAU * f0_hz / f64::from(rate)
        };
        let k = harmonics.len();
        let top = harmonics.iter().copied().max().unwrap_or(0);
        let distinct = {
            let mut sorted = harmonics.to_vec();
            sorted.sort_unstable();
            sorted.windows(2).all(|w| w[0] != w[1])
        };
        let usable = len > 0
            && k > 0
            && rate > 0
            && w0.is_finite()
            && top < (1 << 20)
            && distinct
            && !harmonics.contains(&0);
        let mut fitter = HarmonicFitter {
            len,
            w0,
            harmonics: harmonics.to_vec(),
            bands: Vec::new(),
            operator: None,
        };
        if !usable {
            return fitter;
        }
        // c[n] = Σ cos(n·ω0·t) and s[n] = Σ sin(n·ω0·t) over the window.
        let (mut c, mut s) = (vec![0.0f64; 2 * top + 1], vec![0.0f64; 2 * top + 1]);
        for t in 0..len {
            let (si, co) = (w0 * t as f64).sin_cos();
            let step = Complex64::new(co, si);
            let mut z = Complex64::new(1.0, 0.0);
            for (cn, sn) in c.iter_mut().zip(&mut s) {
                *cn += z.re;
                *sn += z.im;
                z *= step;
            }
        }
        let mut order: Vec<usize> = (0..k).collect();
        order.sort_by_key(|&i| harmonics[i]);
        fitter.bands = order
            .chunks(band.max(1))
            .map(|members| {
                let hs: Vec<usize> = members.iter().map(|&i| harmonics[i]).collect();
                Band {
                    members: members.to_vec(),
                    factor: normal_factor(&hs, &c, &s),
                }
            })
            .collect();
        if fitter.bands.len() > 1 {
            fitter.operator = Some(Operator::new(&c, &s, top));
        }
        fitter
    }

    /// The complex amplitude of each harmonic for each window, as [`harmonic_fit`] gives it:
    /// `x[t] ≈ Re Σ c·e^{i·h·ω0·t}` with `t = 0` at the window's first sample. A window shorter
    /// than the fitter's length is read as if padded with zeros, and a longer one is cut. A
    /// singular system gives all zeros.
    pub fn fit(&self, windows: &[&[f32]]) -> Vec<Vec<Complex64>> {
        let k = self.harmonics.len();
        let zeros = vec![Complex64::default(); k];
        if self.bands.is_empty() {
            return vec![zeros; windows.len()];
        }
        let top = self.harmonics.iter().copied().max().unwrap_or(0);
        // rhs[w][h] = Σ x_w[t]·e^{−i·h·ω0·t}, for every harmonic number up to the highest.
        let mut rhs = vec![vec![Complex64::default(); top + 1]; windows.len()];
        let mut powers = vec![Complex64::default(); top + 1];
        for t in 0..self.len {
            let (si, co) = (self.w0 * t as f64).sin_cos();
            let step = Complex64::new(co, -si);
            let mut z = Complex64::new(1.0, 0.0);
            for p in &mut powers {
                *p = z;
                z *= step;
            }
            for (acc, x) in rhs.iter_mut().zip(windows) {
                let v = f64::from(x.get(t).copied().unwrap_or(0.0));
                if v != 0.0 {
                    acc.iter_mut().zip(&powers).for_each(|(a, p)| *a += p * v);
                }
            }
        }
        rhs.iter()
            .map(|r| {
                // Column j is cos(h·ω0·t), so b = Σ x·cos = Re; column k + j is −sin, so
                // b = −Σ x·sin = Im of Σ x·e^{−i·h·ω0·t}.
                let mut b = vec![0.0f64; 2 * k];
                for (j, &h) in self.harmonics.iter().enumerate() {
                    b[j] = r[h].re;
                    b[k + j] = r[h].im;
                }
                match self.solve(&b) {
                    Some(y) => (0..k).map(|j| Complex64::new(y[j], y[k + j])).collect(),
                    None => zeros.clone(),
                }
            })
            .collect()
    }

    /// The band solves: each band's block of the matrix inverted on its own part of `r`.
    /// `None` when a band is singular.
    fn precondition(&self, r: &[f64]) -> Option<Vec<f64>> {
        let k = self.harmonics.len();
        let mut z = vec![0.0; 2 * k];
        for band in &self.bands {
            let n = band.members.len();
            let mut b = vec![0.0; 2 * n];
            for (j, &i) in band.members.iter().enumerate() {
                b[j] = r[i];
                b[n + j] = r[k + i];
            }
            let y = cholesky_substitute(band.factor.as_ref()?, &b, 2 * n)?;
            for (j, &i) in band.members.iter().enumerate() {
                z[i] = y[j];
                z[k + i] = y[n + j];
            }
        }
        Some(z)
    }

    /// Solves the normal equations for the right-hand side `b`: directly with one band, by
    /// preconditioned conjugate gradients with more. `None` when it cannot be solved.
    fn solve(&self, b: &[f64]) -> Option<Vec<f64>> {
        let mut y = self.precondition(b)?;
        let Some(op) = &self.operator else {
            return Some(y);
        };
        let dot = |u: &[f64], v: &[f64]| u.iter().zip(v).map(|(a, b)| a * b).sum::<f64>();
        let limit = TOLERANCE * dot(b, b).sqrt();
        let gy = op.apply(&self.harmonics, &y);
        let mut r: Vec<f64> = b.iter().zip(&gy).map(|(b, g)| b - g).collect();
        let mut z = self.precondition(&r)?;
        let mut p = z.clone();
        let mut rz = dot(&r, &z);
        for _ in 0..MAX_ITERATIONS {
            if dot(&r, &r).sqrt() <= limit {
                break;
            }
            let gp = op.apply(&self.harmonics, &p);
            let curve = dot(&p, &gp);
            if curve.is_nan() || curve <= 0.0 {
                break;
            }
            let alpha = rz / curve;
            y.iter_mut().zip(&p).for_each(|(y, p)| *y += alpha * p);
            r.iter_mut().zip(&gp).for_each(|(r, g)| *r -= alpha * g);
            z = self.precondition(&r)?;
            let next = dot(&r, &z);
            let beta = next / rz;
            rz = next;
            p.iter_mut().zip(&z).for_each(|(p, z)| *p = z + beta * *p);
        }
        y.iter().all(|v| v.is_finite()).then_some(y)
    }
}

/// The Cholesky factor of the normal matrix for the harmonic numbers `hs`, built from the
/// window sums `c[n] = Σ cos(n·ω0·t)` and `s[n] = Σ sin(n·ω0·t)`. `None` when it is singular.
fn normal_factor(hs: &[usize], c: &[f64], s: &[f64]) -> Option<Vec<f64>> {
    let k = hs.len();
    let m = 2 * k;
    let mut a = vec![0.0f64; m * m];
    for (i, &hi) in hs.iter().enumerate() {
        for (j, &hj) in hs.iter().enumerate() {
            let (diff, sum) = (hi.abs_diff(hj), hi + hj);
            // cos·cos, and (−sin)·(−sin).
            a[i * m + j] = 0.5 * (c[diff] + c[sum]);
            a[(k + i) * m + k + j] = 0.5 * (c[diff] - c[sum]);
            // cos(hi·ω0·t)·(−sin(hj·ω0·t)) = −½(sin((hi + hj)·ω0·t) + sin((hj − hi)·ω0·t)).
            let odd = if hj >= hi { s[diff] } else { -s[diff] };
            let v = -0.5 * (s[sum] + odd);
            a[i * m + k + j] = v;
            a[(k + j) * m + i] = v;
        }
    }
    for i in 0..m {
        a[i * m + i] += RIDGE;
    }
    cholesky_factor(&mut a, m).then_some(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    #[test]
    fn harmonic_fit_recovers_amplitudes_and_phases() {
        let rate = 48_000;
        let f0 = 131.0;
        let parts = [(1.0, 0.3, 0.4), (2.0, 0.2, -1.9), (3.0, 0.05, 2.8)];
        let n = 2048;
        let x: Vec<f32> = (0..n)
            .map(|t| {
                parts
                    .iter()
                    .map(|&(h, a, ph)| a * (TAU * f0 * h * t as f64 / rate as f64 + ph).cos())
                    .sum::<f64>() as f32
            })
            .collect();
        let freqs: Vec<f64> = parts.iter().map(|p| p.0 * f0).collect();
        let c = harmonic_fit(&x, &freqs, rate);
        assert_eq!(c.len(), 3);
        for (c, &(_, a, ph)) in c.iter().zip(&parts) {
            let want = Complex64::from_polar(a, ph);
            assert!((c - want).norm() < 1e-3, "{c} vs {want}");
        }
    }

    #[test]
    fn harmonic_fit_handles_degenerate_input() {
        let zero = Complex64::new(0.0, 0.0);
        assert!(
            harmonic_fit(&[], &[100.0], 48_000)
                .iter()
                .all(|c| *c == zero)
        );
        assert!(
            harmonic_fit(&[0.5; 64], &[100.0], 0)
                .iter()
                .all(|c| *c == zero)
        );
        assert!(harmonic_fit(&[0.5; 64], &[], 48_000).is_empty());
        // The same frequency twice: the ridge keeps it finite.
        let c = harmonic_fit(&[0.5; 64], &[100.0, 100.0, 0.0], 48_000);
        assert!(c.iter().all(|c| c.re.is_finite() && c.im.is_finite()));
    }

    #[test]
    fn harmonic_fitter_matches_the_direct_fit() {
        let rate = 22_050;
        let f0 = 97.3;
        let mut r = crate::dsp::rng::SplitMix64::new(4);
        let n = 1500;
        let x: Vec<f32> = (0..n)
            .map(|t| {
                let tone: f64 = (1..=40)
                    .map(|h| {
                        let w = TAU * f0 * h as f64 * t as f64 / f64::from(rate);
                        (w + 0.3 * h as f64).cos() / h as f64
                    })
                    .sum();
                tone as f32 + 0.1 * (r.next_f32() - 0.5)
            })
            .collect();
        // Not every harmonic, to check gaps in the harmonic numbers.
        let hs: Vec<usize> = (1..=40).filter(|h| h % 7 != 0).collect();
        let freqs: Vec<f64> = hs.iter().map(|&h| h as f64 * f0).collect();
        let fitter = HarmonicFitter::new(n, f0, &hs, rate);
        // A short window reads as zero-padded.
        let mut padded = x[..1000].to_vec();
        padded.resize(n, 0.0);
        let fast = fitter.fit(&[&x, &x[..1000]]);
        for (want, got) in [
            harmonic_fit(&x, &freqs, rate),
            harmonic_fit(&padded, &freqs, rate),
        ]
        .iter()
        .zip(&fast)
        {
            assert_eq!(got.len(), hs.len());
            for (a, b) in want.iter().zip(got) {
                assert!((a - b).norm() < 1e-9, "{a} vs {b}");
            }
        }
        // Degenerate input gives zeros, never a panic.
        let all_zero = |f: HarmonicFitter| f.fit(&[&x]).iter().flatten().all(|c| c.norm() == 0.0);
        assert!(all_zero(HarmonicFitter::new(0, f0, &hs, rate)));
        assert!(all_zero(HarmonicFitter::new(n, f0, &hs, 0)));
        assert!(all_zero(HarmonicFitter::new(n, f64::NAN, &hs, rate)));
        assert!(HarmonicFitter::new(n, f0, &[], rate).fit(&[&x])[0].is_empty());
    }

    /// Harmonics `1..=count` of `f0` with amplitude `1/√h` and scattered phases, plus noise.
    fn bright(f0: f64, count: usize, n: usize, rate: u32) -> Vec<f32> {
        let mut r = crate::dsp::rng::SplitMix64::new(8);
        (0..n)
            .map(|t| {
                let tone: f64 = (1..=count)
                    .map(|h| {
                        let w = TAU * f0 * h as f64 * t as f64 / f64::from(rate);
                        (w + 0.37 * (h * h) as f64).cos() / (h as f64).sqrt()
                    })
                    .sum();
                (0.05 * tone) as f32 + 0.01 * (r.next_f32() - 0.5)
            })
            .collect()
    }

    /// The largest error of `band` against `joint`, relative to each joint amplitude.
    fn worst_relative(joint: &[Complex64], band: &[Complex64]) -> f64 {
        joint
            .iter()
            .zip(band)
            .map(|(j, b)| (j - b).norm() / j.norm())
            .fold(0.0, f64::max)
    }

    #[test]
    fn harmonic_fitter_bands_agree_with_the_joint_fit() {
        let rate = 22_050;
        // 299 harmonics of 36.75 Hz up to 10.99 kHz, over 4.3 periods (not a whole number):
        // three default bands.
        let (f0, count, n) = (36.75, 299, 2600);
        let x = bright(f0, count, n, rate);
        let hs: Vec<usize> = (1..=count).collect();
        let joint = HarmonicFitter::with_band_size(n, f0, &hs, rate, usize::MAX).fit(&[&x]);
        let banded = HarmonicFitter::new(n, f0, &hs, rate);
        assert_eq!(banded.bands.len(), 3);
        let banded = banded.fit(&[&x]);
        let worst = worst_relative(&joint[0], &banded[0]);
        assert!(worst < 1e-6, "default bands: worst relative error {worst}");

        // Small bands, so most harmonics sit near a band edge, and harmonics listed out of
        // order with gaps.
        let hs: Vec<usize> = (1..=100).rev().filter(|h| h % 9 != 0).collect();
        let joint = HarmonicFitter::with_band_size(n, f0, &hs, rate, usize::MAX).fit(&[&x]);
        let small = HarmonicFitter::with_band_size(n, f0, &hs, rate, 32);
        assert_eq!(small.bands.len(), 3);
        let worst = worst_relative(&joint[0], &small.fit(&[&x])[0]);
        assert!(
            worst < 1e-6,
            "32-harmonic bands: worst relative error {worst}"
        );
    }
}
