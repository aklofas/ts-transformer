//! ST 0605 typed model — `TimeStatus` and `PrecisionTimeStampPack`.

/// Time Status byte per MISB ST 0603 §7.4 Table 3.
///
/// - bit 7: 0 = Locked, 1 = Lock Unknown
/// - bit 6: 0 = Normal, 1 = Discontinuity
/// - bit 5: 0 = Forward, 1 = Reverse (only meaningful when bit 6=1)
/// - bits 4-0: reserved, must be 0b11111
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeStatus(pub u8);

impl TimeStatus {
    /// True if bit 7 = 0 (clock locked to absolute time reference).
    pub const fn is_locked(self) -> bool {
        self.0 & 0x80 == 0
    }

    /// True if bit 6 = 1 (time has not incremented forward in a linear
    /// fashion — i.e., a reset, jump, or correction occurred).
    pub const fn has_discontinuity(self) -> bool {
        self.0 & 0x40 != 0
    }

    /// True if bit 5 = 1 (only meaningful when `has_discontinuity()` —
    /// indicates a backward time jump rather than forward).
    pub const fn is_reverse_jump(self) -> bool {
        self.0 & 0x20 != 0
    }

    /// True if reserved bits 4-0 are the spec-required `0b11111`.
    pub const fn reserved_bits_valid(self) -> bool {
        self.0 & 0x1F == 0x1F
    }
}

crate::binding_record! {
    /// MISB ST 0605 §7 Precision Time Stamp Pack typed view.
    ///
    /// Wire form is 26 bytes: the 16-byte universal label, a 1-byte BER length,
    /// the 1-byte time status and the 8-byte big-endian microsecond timestamp.
    #[must_use]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PrecisionTimeStampPack {
        @[custom]
        pub time_status: TimeStatus,
        /// Microseconds since 1970-01-01T00:00:00Z (POSIX epoch), big-endian.
        pub timestamp_us: u64,
    }
}
