//! Big-endian slice accessors. All MFS on-disk integers are big-endian.

pub(crate) fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}
