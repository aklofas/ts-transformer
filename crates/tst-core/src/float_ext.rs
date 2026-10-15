//! Float-math shim for the `no_std` build.
//!
//! `core` exposes no transcendental/rounding float intrinsics (those live in
//! the platform libm that `std` links). Under `no_std` we route through the
//! `libm` crate; under `std` we forward to the inherent `f64` methods. Call sites use the
//! qualified form (`FloatExt::log2(x)`) because inherent methods win method-call
//! resolution whenever std is linked, so the no_std test plane exercises the
//! libm path.

/// Float operations the KLV IMAPB / ST 0601 mapping code needs that are not
/// available on `f64` in a `no_std` (`core`-only) build.
pub(crate) trait FloatExt {
    fn log2(self) -> Self;
    fn ceil(self) -> Self;
    fn floor(self) -> Self;
    fn round(self) -> Self;
    fn powf(self, n: Self) -> Self;
}

#[cfg(feature = "std")]
impl FloatExt for f64 {
    fn log2(self) -> Self {
        f64::log2(self)
    }
    fn ceil(self) -> Self {
        f64::ceil(self)
    }
    fn floor(self) -> Self {
        f64::floor(self)
    }
    fn round(self) -> Self {
        f64::round(self)
    }
    fn powf(self, n: Self) -> Self {
        f64::powf(self, n)
    }
}

#[cfg(not(feature = "std"))]
impl FloatExt for f64 {
    fn log2(self) -> Self {
        libm::log2(self)
    }
    fn ceil(self) -> Self {
        libm::ceil(self)
    }
    fn floor(self) -> Self {
        libm::floor(self)
    }
    fn round(self) -> Self {
        libm::round(self)
    }
    fn powf(self, n: Self) -> Self {
        libm::pow(self, n)
    }
}
