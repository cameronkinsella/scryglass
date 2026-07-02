//! Resampling kernel weights and the sRGB transfer pair, shared by the image
//! and video shaders.
//!
//! Both shaders downscale by summing source taps weighted by a kernel that widens
//! with the downscale ratio (the "factor-aware" prefilter). The weight math is
//! identical on both, so it lives here once, as pure arithmetic that compiles to
//! SPIR-V and also runs under a host `cargo test`.
//!
//! Sources for the kernels:
//! - Mitchell-Netravali cubic family: Mitchell & Netravali, "Reconstruction
//!   Filters in Computer Graphics" (SIGGRAPH 1988). Mitchell = (1/3, 1/3),
//!   Catmull-Rom = (0, 1/2), B-spline = (1, 0).
//!   https://en.wikipedia.org/wiki/Mitchell%E2%80%93Netravali_filters
//! - Lanczos windowed sinc:
//!   https://en.wikipedia.org/wiki/Lanczos_resampling

#![cfg_attr(target_arch = "spirv", no_std)]

use core::f32::consts::PI;

/// Mitchell-Netravali cubic weight at signed distance `x` (in source-pixel units),
/// parameterized by `b` and `c`. Support is `|x| < 2`. `c == 0` has no negative
/// lobes, so it never rings. Larger `c` sharpens at the cost of a small overshoot.
pub fn cubic_weight(x: f32, b: f32, c: f32) -> f32 {
    let x = abs(x);
    let x2 = x * x;
    let x3 = x2 * x;
    if x < 1.0 {
        ((12.0 - 9.0 * b - 6.0 * c) * x3 + (-18.0 + 12.0 * b + 6.0 * c) * x2 + (6.0 - 2.0 * b))
            / 6.0
    } else if x < 2.0 {
        ((-b - 6.0 * c) * x3
            + (6.0 * b + 30.0 * c) * x2
            + (-12.0 * b - 48.0 * c) * x
            + (8.0 * b + 24.0 * c))
            / 6.0
    } else {
        0.0
    }
}

/// Lanczos weight with radius 3 at signed distance `x` (in source-pixel units).
/// Sharpest of the kernels here, with visible ringing, so it is offered for stills
/// only. Support is `|x| < 3`.
pub fn lanczos3_weight(x: f32) -> f32 {
    if abs(x) < 3.0 {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// Normalized sinc `sin(pi x) / (pi x)`, with the removable singularity at 0.
fn sinc(x: f32) -> f32 {
    if x == 0.0 {
        1.0
    } else {
        let px = PI * x;
        libm::sinf(px) / px
    }
}

/// Branchless `abs` avoids `f32::abs`, which is std-only and so absent under the
/// shader's `no_std`.
fn abs(x: f32) -> f32 {
    if x < 0.0 { -x } else { x }
}

/// sRGB gamma to linear light for one channel, per IEC 61966-2-1.
/// https://en.wikipedia.org/wiki/SRGB#Transfer_function_(%22gamma%22)
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        libm::powf((c + 0.055) / 1.055, 2.4)
    }
}

/// Linear light to sRGB gamma for one channel, the inverse of [`srgb_to_linear`].
pub fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        libm::powf(c, 1.0 / 2.4) * 1.055 - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MITCHELL: (f32, f32) = (1.0 / 3.0, 1.0 / 3.0);
    const CATMULL_ROM: (f32, f32) = (0.0, 0.5);
    const B_SPLINE: (f32, f32) = (1.0, 0.0);

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn cubic_is_symmetric() {
        for &(b, c) in &[MITCHELL, CATMULL_ROM, B_SPLINE] {
            for &x in &[0.3, 0.75, 1.2, 1.9, 2.5] {
                assert!(close(cubic_weight(x, b, c), cubic_weight(-x, b, c)));
            }
        }
    }

    #[test]
    fn cubic_vanishes_beyond_its_support() {
        for &(b, c) in &[MITCHELL, CATMULL_ROM, B_SPLINE] {
            assert_eq!(cubic_weight(2.0, b, c), 0.0);
            assert_eq!(cubic_weight(2.5, b, c), 0.0);
            assert_eq!(cubic_weight(100.0, b, c), 0.0);
        }
    }

    #[test]
    fn cubic_is_a_partition_of_unity_at_integer_offsets() {
        // At a texel center the contributing offsets are integers, and any usable
        // reconstruction kernel sums to 1 there. This exercises both polynomial
        // pieces at once, so it pins the coefficients.
        for &(b, c) in &[MITCHELL, CATMULL_ROM, B_SPLINE] {
            let sum = cubic_weight(0.0, b, c) + 2.0 * cubic_weight(1.0, b, c);
            assert!(close(sum, 1.0), "sum {sum} for ({b}, {c})");
        }
    }

    #[test]
    fn cubic_pieces_meet_continuously_at_one() {
        // Both polynomial pieces equal b/6 at the x = 1 seam, so the near-side and
        // far-side values converge as the step shrinks (a plain C0 check).
        let h = 1e-4;
        for &(b, c) in &[MITCHELL, CATMULL_ROM, B_SPLINE] {
            assert!((cubic_weight(1.0 - h, b, c) - cubic_weight(1.0 + h, b, c)).abs() < 1e-3);
        }
    }

    #[test]
    fn catmull_rom_interpolates() {
        // Catmull-Rom passes through the samples: full weight at the center, zero at
        // every other integer offset.
        assert!(close(cubic_weight(0.0, CATMULL_ROM.0, CATMULL_ROM.1), 1.0));
        assert!(close(cubic_weight(1.0, CATMULL_ROM.0, CATMULL_ROM.1), 0.0));
        assert!(close(cubic_weight(2.0, CATMULL_ROM.0, CATMULL_ROM.1), 0.0));
    }

    #[test]
    fn catmull_rom_rings_but_a_zero_c_kernel_does_not() {
        // c > 0 buys sharpness with a negative lobe (ringing). c == 0 stays
        // non-negative, which is why the no-ring default lives in that family.
        assert!(cubic_weight(1.5, CATMULL_ROM.0, CATMULL_ROM.1) < 0.0);
        for i in 0..=40 {
            let x = i as f32 * 0.05;
            assert!(cubic_weight(x, B_SPLINE.0, B_SPLINE.1) >= 0.0, "x {x}");
        }
    }

    #[test]
    fn lanczos_is_one_at_center_and_zero_at_nonzero_integers() {
        assert!(close(lanczos3_weight(0.0), 1.0));
        for x in [1.0, 2.0, 3.0] {
            assert!(close(lanczos3_weight(x), 0.0), "x {x}");
        }
    }

    #[test]
    fn lanczos_vanishes_beyond_its_support() {
        assert_eq!(lanczos3_weight(3.0), 0.0);
        assert_eq!(lanczos3_weight(3.5), 0.0);
        assert_eq!(lanczos3_weight(-4.0), 0.0);
    }

    #[test]
    fn lanczos_is_symmetric_and_rings() {
        for x in [0.4, 1.5, 2.7] {
            assert!(close(lanczos3_weight(x), lanczos3_weight(-x)));
        }
        assert!(lanczos3_weight(1.5) < 0.0);
    }

    #[test]
    fn srgb_transfer_matches_the_iec_reference_points() {
        // Below the breakpoint the curve is linear.
        assert!(close(srgb_to_linear(0.04045), 0.04045 / 12.92));
        assert!(close(linear_to_srgb(0.003_130_8), 0.003_130_8 * 12.92));
        // 18% gray and mid-gray, IEC 61966-2-1 curve.
        assert!(close(srgb_to_linear(0.5), 0.214_041_14));
        assert!(close(linear_to_srgb(0.18), 0.461_356_13));
        assert!(close(srgb_to_linear(0.0), 0.0));
        assert!(close(srgb_to_linear(1.0), 1.0));
    }

    #[test]
    fn srgb_transfer_round_trips() {
        for c in [0.0, 0.002, 0.04, 0.1, 0.5, 0.73, 1.0] {
            assert!(close(linear_to_srgb(srgb_to_linear(c)), c), "c {c}");
            assert!(close(srgb_to_linear(linear_to_srgb(c)), c), "c {c}");
        }
    }
}
