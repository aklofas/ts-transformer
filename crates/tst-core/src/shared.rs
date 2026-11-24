//! `SharedBytes` — a refcounted, cheaply-cloneable, sub-sliceable byte buffer.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! One backing `Arc<[u8]>` allocation is shared by all clones and sub-slices, so a
//! demuxed access unit can be held once while parsed NAL/OBU bodies are zero-copy
//! windows into it. Derefs to `[u8]`, so `&buf[..]` / `&*buf` read sites are unchanged.
//!
//! no_std: uses `alloc::sync::Arc`, available on the workspace's bare-metal targets
//! (thumbv7em-none-eabihf and riscv32imac both have native atomics).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::{Deref, Range};

#[derive(Clone)]
pub struct SharedBytes {
    buf: Arc<[u8]>,
    off: usize,
    len: usize,
}

impl SharedBytes {
    /// Wrap an owned `Vec<u8>` in a single shared allocation.
    pub fn from_vec(v: Vec<u8>) -> Self {
        let len = v.len();
        Self {
            buf: Arc::from(v.into_boxed_slice()),
            off: 0,
            len,
        }
    }

    /// Build a `SharedBytes` directly from a borrowed byte slice (one copy
    /// into a fresh `Arc<[u8]>`). Prefer this over `from_vec` when the
    /// caller has a `&[u8]` and does not need to consume an owned `Vec` — it
    /// avoids an extra Box intermediary that `from_vec` would otherwise
    /// produce when the Vec is not at capacity.
    pub fn from_slice(s: &[u8]) -> Self {
        let len = s.len();
        Self {
            buf: Arc::from(s),
            off: 0,
            len,
        }
    }

    /// A zero-copy sub-window sharing the same allocation.
    ///
    /// Panics if `range` is outside the current window (mirrors slice indexing).
    pub fn slice(&self, range: Range<usize>) -> Self {
        assert!(
            range.start <= range.end && range.end <= self.len,
            "slice {range:?} out of range for window of length {}",
            self.len
        );
        Self {
            buf: Arc::clone(&self.buf),
            off: self.off + range.start,
            len: range.end - range.start,
        }
    }

    /// The current window as a `&[u8]`. Prefer `Deref` (`&*buf` / `&buf[..]`) in
    /// most cases; use this when you need the `&[u8]` explicitly (e.g. a generic bound).
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[self.off..self.off + self.len]
    }
}

impl Deref for SharedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<Vec<u8>> for SharedBytes {
    fn from(v: Vec<u8>) -> Self {
        Self::from_vec(v)
    }
}

impl PartialEq for SharedBytes {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for SharedBytes {}

impl core::hash::Hash for SharedBytes {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl core::fmt::Debug for SharedBytes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SharedBytes(len={})", self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::SharedBytes;
    use alloc::vec;

    #[test]
    fn from_vec_derefs_to_slice() {
        let b = SharedBytes::from_vec(vec![1, 2, 3, 4]);
        assert_eq!(&*b, &[1, 2, 3, 4]);
        assert_eq!(b.len(), 4);
    }

    #[test]
    fn slice_is_a_zero_copy_window_sharing_the_allocation() {
        let b = SharedBytes::from_vec(vec![10, 11, 12, 13, 14]);
        let mid = b.slice(1..4);
        assert_eq!(&*mid, &[11, 12, 13]);
        assert_eq!(mid.as_ptr(), b[1..].as_ptr(), "the slice copied its bytes");
        let c = mid.clone();
        assert_eq!(&*c, &[11, 12, 13]);
        assert_eq!(c.as_ptr(), b[1..].as_ptr(), "the clone copied its bytes");
    }

    #[test]
    fn equality_is_by_content_not_identity() {
        let a = SharedBytes::from_vec(vec![1, 2, 3]);
        let b = SharedBytes::from_vec(vec![1, 2, 3]);
        assert_eq!(a, b);
        assert_eq!(a, a.slice(0..3));
    }

    #[test]
    fn nested_slice_compounds_offsets() {
        let b = SharedBytes::from_vec(vec![10, 11, 12, 13, 14]);
        let inner = b.slice(1..4).slice(1..3); // -> [12, 13]
        assert_eq!(&*inner, &[12, 13]);
    }

    #[test]
    #[should_panic]
    fn slice_out_of_range_panics() {
        let b = SharedBytes::from_vec(vec![1, 2]);
        let _ = b.slice(0..5);
    }
}
