//! Distance kernels: scalar reference implementations and AVX2+FMA versions
//! selected at runtime. All distances follow "smaller is closer":
//!
//! * `L2`      squared Euclidean distance
//! * `Ip`      negative inner product
//! * `Cosine`  1 - cos(a, b); vectors are L2-normalised on insert, so this is
//!   computed as 1 - dot(a, b)
//!
//! The SIMD kernels are tested against the scalar ones on random inputs of
//! every length 0..300 (tail handling) and with a relative tolerance, since
//! summation order differs.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    L2,
    Ip,
    Cosine,
}

impl Metric {
    pub fn as_u8(self) -> u8 {
        match self {
            Metric::L2 => 0,
            Metric::Ip => 1,
            Metric::Cosine => 2,
        }
    }
    pub fn from_u8(v: u8) -> Option<Metric> {
        match v {
            0 => Some(Metric::L2),
            1 => Some(Metric::Ip),
            2 => Some(Metric::Cosine),
            _ => None,
        }
    }

    #[inline]
    pub fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            Metric::L2 => l2_sq(a, b),
            Metric::Ip => -dot(a, b),
            Metric::Cosine => 1.0 - dot(a, b),
        }
    }
}

// ---------------------------------------------------------------- scalar

pub fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn l2_sq_scalar(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

// ---------------------------------------------------------------- dispatch

#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    use std::sync::OnceLock;
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma"))
}

#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dimension mismatch");
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: the CPU supports AVX2 and FMA (checked at runtime) and the
        // slices have equal length (asserted above).
        return unsafe { avx2::dot(a, b) };
    }
    dot_scalar(a, b)
}

#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dimension mismatch");
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: as in `dot`.
        return unsafe { avx2::l2_sq(a, b) };
    }
    l2_sq_scalar(a, b)
}

/// Which kernel the dispatcher uses on this machine (reported by benchmarks).
pub fn kernel_name() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        return "avx2+fma";
    }
    "scalar"
}

/// L2-normalises `v` in place; false for a zero (or non-finite) vector.
/// Scales by the largest magnitude first, so a finite vector whose squared
/// norm would overflow f32 still normalises correctly.
pub fn normalize(v: &mut [f32]) -> bool {
    let scale = v.iter().fold(0.0f32, |a, x| a.max(x.abs()));
    if scale == 0.0 || !scale.is_finite() {
        return false;
    }
    for x in v.iter_mut() {
        *x /= scale;
    }
    let n = dot_scalar(v, v).sqrt();
    if n == 0.0 || !n.is_finite() {
        return false;
    }
    for x in v.iter_mut() {
        *x /= n;
    }
    true
}

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn hsum(v: __m256) -> f32 {
        let lo = _mm256_castps256_ps128(v);
        let hi = _mm256_extractf128_ps(v, 1);
        let s = _mm_add_ps(lo, hi);
        let s = _mm_add_ps(s, _mm_movehl_ps(s, s));
        let s = _mm_add_ss(s, _mm_shuffle_ps(s, s, 0x55));
        _mm_cvtss_f32(s)
    }

    /// # Safety
    /// Requires AVX2+FMA and `a.len() == b.len()`.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();
        let mut i = 0;
        while i + 16 <= n {
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc0);
            acc1 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i + 8)), _mm256_loadu_ps(pb.add(i + 8)), acc1);
            i += 16;
        }
        if i + 8 <= n {
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc0);
            i += 8;
        }
        let mut s = hsum(_mm256_add_ps(acc0, acc1));
        while i < n {
            s += *pa.add(i) * *pb.add(i);
            i += 1;
        }
        s
    }

    /// # Safety
    /// Requires AVX2+FMA and `a.len() == b.len()`.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();
        let mut i = 0;
        while i + 16 <= n {
            let d0 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            let d1 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i + 8)), _mm256_loadu_ps(pb.add(i + 8)));
            acc0 = _mm256_fmadd_ps(d0, d0, acc0);
            acc1 = _mm256_fmadd_ps(d1, d1, acc1);
            i += 16;
        }
        if i + 8 <= n {
            let d0 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            acc0 = _mm256_fmadd_ps(d0, d0, acc0);
            i += 8;
        }
        let mut s = hsum(_mm256_add_ps(acc0, acc1));
        while i < n {
            let d = *pa.add(i) - *pb.add(i);
            s += d * d;
            i += 1;
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() <= 1e-4 * (1.0 + a.abs().max(b.abs()))
    }

    #[test]
    fn simd_matches_scalar_for_every_length() {
        let mut rng = rand::rngs::SmallRng::seed_from_u64(1);
        for n in 0..300 {
            for _ in 0..5 {
                let a: Vec<f32> = (0..n).map(|_| rng.gen_range(-10.0..10.0)).collect();
                let b: Vec<f32> = (0..n).map(|_| rng.gen_range(-10.0..10.0)).collect();
                assert!(close(dot(&a, &b), dot_scalar(&a, &b)), "dot n={n}");
                assert!(close(l2_sq(&a, &b), l2_sq_scalar(&a, &b)), "l2 n={n}");
            }
        }
    }

    #[test]
    fn metric_semantics() {
        let a = [1.0, 0.0];
        let b = [0.0, 1.0];
        assert_eq!(Metric::L2.distance(&a, &b), 2.0);
        assert_eq!(Metric::Ip.distance(&a, &a), -1.0);
        assert!(Metric::Cosine.distance(&a, &b) - 1.0 < 1e-6);
        let mut z = [0.0f32; 3];
        assert!(!normalize(&mut z));
    }

    #[test]
    #[should_panic(expected = "dimension mismatch")]
    fn mismatched_dims_panic() {
        dot(&[1.0, 2.0], &[1.0]);
    }
}
