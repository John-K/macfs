//! Shared helpers for the black-box integration suites.
//!
//! `macfs` has no dependencies and neither do its tests, so the pseudo-random
//! generator and the CRC-32 used to fingerprint fork contents are hand-rolled
//! here. Both are deterministic: a test that fails once fails the same way
//! forever.

// Each integration binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::path::PathBuf;

// ------------------------------------------------------------------ xorshift64

/// A xorshift64 pseudo-random generator (Marsaglia's 13/7/17 triple).
///
/// Not cryptographic and not meant to be — it exists to fill forks with bytes
/// that make a misplaced allocation block impossible to miss.
#[derive(Debug, Clone)]
pub struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    /// Seed the generator. A zero seed is nudged to a non-zero one, because
    /// xorshift is stuck at zero forever.
    pub fn new(seed: u64) -> Self {
        Xorshift64 { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
    }

    /// The next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

/// A closure that yields successive xorshift64 values, for callers that would
/// rather not name the type.
pub fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut prng = Xorshift64::new(seed);
    move || prng.next_u64()
}

/// `len` deterministic bytes derived from `seed`.
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut prng = Xorshift64::new(seed);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(&prng.next_u64().to_le_bytes());
    }
    out.truncate(len);
    out
}

// ---------------------------------------------------------------------- CRC-32

/// CRC-32 as used by IEEE 802.3 / zip / PNG: reflected input and output,
/// polynomial 0xEDB88320, initial and final value 0xFFFFFFFF.
///
/// Bitwise rather than table-driven: the test images are under a megabyte, so
/// the eight-fold cost buys a function short enough to read at a glance.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// ---------------------------------------------------------------- golden images

/// Path to a golden test image, or `None` (with a note on stderr) if it has
/// not been fetched.
///
/// The real images are Apple-copyrighted and never committed, so every golden
/// test early-returns when its image is absent and the suite stays green on a
/// fresh clone.
pub fn image_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/images")
        .join(name);
    if path.is_file() {
        Some(path)
    } else {
        eprintln!("skipping: tests/images/{name} not present — run scripts/fetch-test-images.sh");
        None
    }
}

// ----------------------------------------------------------------------- tests

/// The canonical CRC-32 check value, from the IEEE 802.3 specification.
#[test]
fn crc32_matches_the_standard_check_vector() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
    assert_eq!(crc32(b"a"), 0xE8B7_BE43);
    // Different content must not collide on anything this simple.
    assert_ne!(crc32(b"ab"), crc32(b"ba"));
}

#[test]
fn random_bytes_are_deterministic_and_the_right_length() {
    for len in [0usize, 1, 7, 8, 9, 1024, 100_000] {
        let a = random_bytes(0xC0FF_EE00, len);
        assert_eq!(a.len(), len);
        assert_eq!(a, random_bytes(0xC0FF_EE00, len));
        if len > 16 {
            assert_ne!(a, random_bytes(0xC0FF_EE01, len));
        }
    }
    // The closure form walks the same sequence as the struct.
    let mut next = rng(42);
    let mut prng = Xorshift64::new(42);
    for _ in 0..4 {
        assert_eq!(next(), prng.next_u64());
    }
}

#[test]
fn image_path_reports_a_missing_image_instead_of_failing() {
    assert!(image_path("no-such-image-9e3779b9.image").is_none());
}
