//! The vendored librist's per-key rekey budget, driven through its PSK
//! decrypt directly. A nonce that differs from the key's current one makes
//! the decrypt derive a new key (PBKDF2), and the decrypt returns 0 when it
//! decrypted and -1 when it refused, so under ever-new nonces the count of
//! 0 returns is the count of derivations.
//!
//! These are librist internals (hidden visibility, so they resolve only in
//! a static link), and the status return is one of rist-sys's source
//! fixups: the file builds only against the vendored build.
#![cfg(all(feature = "mbedtls", librist_source_fixups))]

// Nothing below names a rist-sys item, so without this the test binary would
// not link librist at all.
extern crate rist_sys;

use std::ffi::{c_char, c_int, c_void};
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn _librist_crypto_psk_rist_key_init(
        key: *mut c_void,
        key_size: u32,
        rotation: u32,
        password: *const c_char,
        odd: bool,
    ) -> c_int;
    fn _librist_crypto_psk_rist_key_destroy(key: *mut c_void) -> c_int;
    fn _librist_crypto_psk_decrypt(
        key: *mut c_void,
        nonce: *mut u8,
        seq_nbe: u32,
        gre_version: u8,
        inbuf: *const u8,
        outbuf: *mut u8,
        payload_len: usize,
    ) -> c_int;
}

/// librist's refill interval and burst (`RIST_REKEY_MIN_INTERVAL` and
/// `RIST_REKEY_BURST` in `src/crypto/psk.c`).
const INTERVAL: Duration = Duration::from_millis(10);
const BURST: u64 = 16;
/// `INTERVAL` rounded down past librist's NTP-unit rounding of it.
const NTP_INTERVAL_FLOOR: Duration = Duration::from_nanos(9_999_999);

/// Room for a `struct rist_key`, which Rust cannot name: zeroed, aligned,
/// and several times the struct's size (under 1 KiB with mbedTLS 3.6).
#[repr(C, align(64))]
struct KeyStorage([u8; 8192]);

/// One AES-256 PSK key slot, as a receiving peer holds it.
struct Key(Box<KeyStorage>);

impl Key {
    fn new() -> Self {
        let mut key = Key(Box::new(KeyStorage([0; 8192])));
        // SAFETY: the storage is zeroed, aligned and larger than the struct;
        // the password is a NUL-terminated literal.
        let rc = unsafe {
            _librist_crypto_psk_rist_key_init(
                key.ptr(),
                256,
                0,
                c"rekey-budget-test-secret".as_ptr(),
                false,
            )
        };
        assert_eq!(rc, 0, "key init");
        key
    }

    fn ptr(&mut self) -> *mut c_void {
        self.0.0.as_mut_ptr().cast()
    }

    /// Decrypt one 16-byte payload under `nonce`; true when the decrypt ran
    /// (and so, under a nonce the key did not hold, derived a key).
    fn decrypt(&mut self, nonce: [u8; 4]) -> bool {
        let mut nonce = nonce;
        let input = [0u8; 16];
        let mut output = [0u8; 16];
        // SAFETY: the key was initialised by `new`; the buffers are 16 bytes.
        let rc = unsafe {
            _librist_crypto_psk_decrypt(
                self.ptr(),
                nonce.as_mut_ptr(),
                1u32.to_be(),
                1,
                input.as_ptr(),
                output.as_mut_ptr(),
                input.len(),
            )
        };
        rc == 0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: initialised by `new`, destroyed once.
        unsafe {
            _librist_crypto_psk_rist_key_destroy(self.ptr());
        }
    }
}

/// A distinct non-zero nonce for each `n`, top (odd-key) bit clear.
fn nonce(n: u32) -> [u8; 4] {
    let b = (n + 1).to_be_bytes();
    [b[0] & 0x7f, b[1], b[2], b[3]]
}

/// A sender that puts a fresh nonce on every datagram used to buy one PBKDF2
/// per datagram: each successful install reset the strike count librist's
/// rekey limit waits for. Every fresh nonce now spends the key's budget.
#[test]
fn a_fresh_nonce_flood_is_held_to_the_rekey_budget() {
    // The cost of one derivation, for the record (a separate key, so the
    // flood below starts with a full budget).
    let mut probe = Key::new();
    let t = Instant::now();
    for n in 0..BURST as u32 {
        assert!(probe.decrypt(nonce(n)), "the burst itself was refused");
    }
    eprintln!(
        "one rekey (PBKDF2) costs about {:?}",
        t.elapsed() / BURST as u32
    );

    let mut key = Key::new();
    const FLOOD: u32 = 2000;
    let start = Instant::now();
    let derived = (0..FLOOD).filter(|&n| key.decrypt(nonce(n))).count() as u64;
    let elapsed = start.elapsed();
    // The burst and one refill per interval that passed. librist's interval
    // is 42 949 670 NTP units, just under 10 ms, so the division uses a
    // divisor just under it too. A slower machine only widens the bound.
    let bound = BURST + (elapsed.as_nanos() / NTP_INTERVAL_FLOOR.as_nanos()) as u64;
    assert!(
        derived >= BURST,
        "only {derived} derivations: the burst was refused"
    );
    assert!(
        derived <= bound,
        "{derived} of {FLOOD} fresh nonces derived a key in {elapsed:?}; the budget allows {bound}"
    );
}

/// From a full budget, a run of fresh nonces quicker than one refill admits
/// exactly the burst and refuses the next.
#[test]
fn a_cold_key_admits_exactly_the_burst() {
    let mut key = Key::new();
    let start = Instant::now();
    let admitted = (0..BURST as u32 + 1)
        .filter(|&n| key.decrypt(nonce(n)))
        .count() as u64;
    let elapsed = start.elapsed();
    if elapsed < Duration::from_millis(9) {
        assert_eq!(admitted, BURST, "a cold key admitted {admitted} rekeys");
    } else {
        // A refill may have landed inside the run; the flood test still
        // holds the upper bound.
        eprintln!(
            "{} rekeys took {elapsed:?}, over one refill: not checked",
            BURST + 1
        );
    }
}

/// A reflector subscriber's key sees the reflector's own RTCP nonce and the
/// forwarded media nonce interleaved: two back-to-back rekeys per RTCP
/// interval (about 100 ms; here 50 ms, twice that rate). None may be refused.
#[test]
fn reflector_nonce_interleave_is_never_refused() {
    let mut key = Key::new();
    let (rtcp, media) = (nonce(1), nonce(2));
    for round in 0..20 {
        assert!(
            key.decrypt(rtcp),
            "round {round}: the RTCP nonce was refused"
        );
        assert!(
            key.decrypt(media),
            "round {round}: the media nonce was refused"
        );
        // Media keeps flowing under its nonce between switches: no rekey.
        for _ in 0..5 {
            assert!(
                key.decrypt(media),
                "round {round}: same-nonce media was refused"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// After a flood empties the budget, a legitimate fresh nonce (a sender's
/// key rotation, or the nonce that clears a lockout) is admitted once the
/// budget refills, and a whole burst is available again later.
#[test]
fn a_fresh_nonce_is_admitted_once_the_budget_refills() {
    let mut key = Key::new();
    for n in 0..200 {
        key.decrypt(nonce(n));
    }
    std::thread::sleep(INTERVAL * 20);
    let rotated = nonce(10_000);
    assert!(
        key.decrypt(rotated),
        "a fresh nonce after the refill was refused"
    );
    assert!(key.decrypt(rotated), "the adopted nonce stopped decrypting");
    std::thread::sleep(INTERVAL * (BURST as u32 + 4));
    let admitted = (0..BURST as u32)
        .filter(|&n| key.decrypt(nonce(20_000 + n)))
        .count() as u64;
    assert_eq!(admitted, BURST, "the budget did not refill to a full burst");
}
