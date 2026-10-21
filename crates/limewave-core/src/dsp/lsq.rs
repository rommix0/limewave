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
    // A = L·Lᵀ, with L stored in the lower triangle.
    for j in 0..m {
        let d = a[j * m + j] - (0..j).map(|p| a[j * m + p] * a[j * m + p]).sum::<f64>();
        if !d.is_finite() || d <= 0.0 {
            return None;
        }
        let d = d.sqrt();
        a[j * m + j] = d;
        for i in j + 1..m {
            let s = a[i * m + j] - (0..j).map(|p| a[i * m + p] * a[j * m + p]).sum::<f64>();
            a[i * m + j] = s / d;
        }
    }
    // L·z = b, then Lᵀ·y = z.
    let mut y = b.to_vec();
    for i in 0..m {
        let s: f64 = (0..i).map(|p| a[i * m + p] * y[p]).sum();
        y[i] = (y[i] - s) / a[i * m + i];
    }
    for i in (0..m).rev() {
        let s: f64 = (i + 1..m).map(|p| a[p * m + i] * y[p]).sum();
        y[i] = (y[i] - s) / a[i * m + i];
    }
    y.iter().all(|v| v.is_finite()).then_some(y)
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
}
