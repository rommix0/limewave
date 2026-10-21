//! Least-squares fits.

use std::f64::consts::TAU;

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

/// The same joint least-squares fit as [`harmonic_fit`], for many harmonics of one fundamental
/// over many windows of one length, at a fraction of the cost.
///
/// The frequencies are `h·f0` for whole harmonic numbers `h`, so every entry of the normal
/// matrix is a sum of `cos(n·ω0·t)` or `sin(n·ω0·t)` over the window for a whole number `n`.
/// Those `2·(max h) + 1` sums are computed once, the matrix is built from them and factorized
/// once (with the same 1e-9 ridge), and each fit then costs one pass over its window plus a
/// substitution. The fit is joint over all the harmonics, exactly as in [`harmonic_fit`].
pub struct HarmonicFitter {
    len: usize,
    w0: f64,
    harmonics: Vec<usize>,
    /// The Cholesky factor of the normal matrix; `None` when it is singular or empty.
    factor: Option<Vec<f64>>,
}

impl HarmonicFitter {
    /// A fitter for windows of `len` samples and the harmonic numbers `harmonics` (each ≥ 1,
    /// no repeats) of `f0_hz`, at `rate`.
    pub fn new(len: usize, f0_hz: f64, harmonics: &[usize], rate: u32) -> HarmonicFitter {
        let w0 = if rate == 0 {
            0.0
        } else {
            TAU * f0_hz / f64::from(rate)
        };
        let k = harmonics.len();
        let m = 2 * k;
        let top = harmonics.iter().copied().max().unwrap_or(0);
        let usable = len > 0 && k > 0 && rate > 0 && w0.is_finite() && top < (1 << 20);
        let factor = if usable {
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
            let mut a = vec![0.0f64; m * m];
            for (i, &hi) in harmonics.iter().enumerate() {
                for (j, &hj) in harmonics.iter().enumerate() {
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
        } else {
            None
        };
        HarmonicFitter {
            len,
            w0,
            harmonics: harmonics.to_vec(),
            factor,
        }
    }

    /// The complex amplitude of each harmonic for each window, as [`harmonic_fit`] gives it:
    /// `x[t] ≈ Re Σ c·e^{i·h·ω0·t}` with `t = 0` at the window's first sample. A window shorter
    /// than the fitter's length is read as if padded with zeros, and a longer one is cut. A
    /// singular system gives all zeros.
    pub fn fit(&self, windows: &[&[f32]]) -> Vec<Vec<Complex64>> {
        let k = self.harmonics.len();
        let Some(factor) = &self.factor else {
            return vec![vec![Complex64::default(); k]; windows.len()];
        };
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
        let m = 2 * k;
        rhs.iter()
            .map(|r| {
                // Column j is cos(h·ω0·t), so b = Σ x·cos = Re; column k + j is −sin, so
                // b = −Σ x·sin = Im of Σ x·e^{−i·h·ω0·t}.
                let mut b = vec![0.0f64; m];
                for (j, &h) in self.harmonics.iter().enumerate() {
                    b[j] = r[h].re;
                    b[k + j] = r[h].im;
                }
                match cholesky_substitute(factor, &b, m) {
                    Some(y) => (0..k).map(|j| Complex64::new(y[j], y[k + j])).collect(),
                    None => vec![Complex64::default(); k],
                }
            })
            .collect()
    }
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
}
