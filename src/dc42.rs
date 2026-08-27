//! DiskCopy 4.2 container.
//!
//! DiskCopy 4.2 is the Apple disk-image format produced by the Disk Copy utility
//! and understood by essentially every Macintosh emulator. It wraps a raw sector
//! image in an 84-byte big-endian header carrying the image name, the data and
//! tag sizes, two checksums, and the floppy geometry.
//!
//! ```text
//! offset  size  field
//!      0    64  image name, Pascal string (length byte <= 63), zero padded
//!     64     4  dataSize      bytes of disk data
//!     68     4  tagSize       bytes of tag data (12 per 512-byte sector, or 0)
//!     72     4  dataChecksum
//!     76     4  tagChecksum
//!     80     1  diskFormat    0 = 400K, 1 = 800K, 2 = 720K, 3 = 1440K
//!     81     1  formatByte    0x02 = 400K, 0x22 = >400K Mac, 0x24 = 720K/1440K
//!     82     2  magic         0x0100
//!     84     n  disk data
//!   84+n     m  tag data
//! ```
//!
//! Tags are advisory sector metadata written by GCR floppy controllers. This
//! module preserves them verbatim so that an open/save cycle is byte-identical.

use crate::error::{MfsError, Result};
use crate::util::{rd_u16, rd_u32, wr_u16, wr_u32};

/// Size of the DiskCopy 4.2 header that precedes the disk data.
pub(crate) const HEADER_LEN: usize = 84;

/// Maximum number of bytes in the header's Pascal-string image name.
const MAX_NAME_LEN: usize = 63;

// Header field offsets.
const OFF_DATA_SIZE: usize = 64;
const OFF_TAG_SIZE: usize = 68;
const OFF_DATA_CKSUM: usize = 72;
const OFF_TAG_CKSUM: usize = 76;
const OFF_DISK_FORMAT: usize = 80;
const OFF_FORMAT_BYTE: usize = 81;
const OFF_MAGIC: usize = 82;

/// The header magic word stored at [`OFF_MAGIC`].
const MAGIC: u16 = 0x0100;

/// The number of leading tag bytes excluded from `tagChecksum`.
///
/// Disk Copy 4.2 skips the tag bytes belonging to the first sector when
/// checksumming tag data. The quirk is undocumented by Apple but universal
/// across implementations, so it must be reproduced to match real images.
const TAG_CHECKSUM_SKIP: usize = 12;

/// Tag bytes per 512-byte sector on a GCR floppy.
const TAG_BYTES_PER_SECTOR: usize = 12;

/// Bytes in a 400K floppy image.
const SIZE_400K: usize = 409_600;
/// Bytes in an 800K floppy image.
const SIZE_800K: usize = 819_200;

/// A parsed DiskCopy 4.2 container.
pub(crate) struct Dc42Image {
    /// Raw Pascal-string content bytes of the image name (at most 63),
    /// preserved verbatim without any character-set interpretation.
    pub name: Vec<u8>,
    /// The disk data — a raw sector image.
    pub data: Vec<u8>,
    /// Tag data, preserved verbatim. May be empty.
    pub tags: Vec<u8>,
    /// `diskFormat`: 0 = 400K, 1 = 800K, 2 = 720K, 3 = 1440K.
    pub disk_format: u8,
    /// `formatByte`: 0x02 for 400K, 0x22 for larger Mac disks, 0x24 for MFM.
    pub format_byte: u8,
}

/// Summarizes the container rather than dumping several hundred kilobytes of
/// disk data, which is what a derived `Debug` would print on a failed assert.
impl std::fmt::Debug for Dc42Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dc42Image")
            .field("name", &String::from_utf8_lossy(&self.name))
            .field("data_len", &self.data.len())
            .field("tags_len", &self.tags.len())
            .field("disk_format", &self.disk_format)
            .field("format_byte", &format_args!("{:#04x}", self.format_byte))
            .finish()
    }
}

/// The DiskCopy 4.2 checksum: a rotating sum over big-endian 16-bit words.
///
/// A trailing odd byte, which cannot occur in a well-formed image, is ignored.
fn checksum(bytes: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    for chunk in bytes.chunks_exact(2) {
        sum = sum.wrapping_add(u16::from_be_bytes([chunk[0], chunk[1]]) as u32);
        sum = sum.rotate_right(1);
    }
    sum
}

/// The checksum stored in `tagChecksum`, which skips the first sector's tags.
fn tag_checksum(tags: &[u8]) -> u32 {
    if tags.len() <= TAG_CHECKSUM_SKIP {
        0
    } else {
        checksum(&tags[TAG_CHECKSUM_SKIP..])
    }
}

/// Validates the structural invariants shared by [`Dc42Image::detect`] and
/// [`Dc42Image::parse`], returning the data and tag sizes as `usize`.
///
/// The size arithmetic is done in `u64` so that a hostile header claiming a
/// 4 GB data size cannot wrap around into a plausible-looking total.
fn validate_header(bytes: &[u8]) -> std::result::Result<(usize, usize), String> {
    if bytes.len() < HEADER_LEN {
        return Err(format!(
            "image is {} bytes, shorter than the {HEADER_LEN}-byte header",
            bytes.len()
        ));
    }
    let magic = rd_u16(bytes, OFF_MAGIC);
    if magic != MAGIC {
        return Err(format!(
            "bad magic {magic:#06x} at offset {OFF_MAGIC} (expected 0x0100)"
        ));
    }
    let name_len = bytes[0] as usize;
    if name_len > MAX_NAME_LEN {
        return Err(format!(
            "image name length {name_len} exceeds the {MAX_NAME_LEN}-byte maximum"
        ));
    }
    let data_size = rd_u32(bytes, OFF_DATA_SIZE) as u64;
    let tag_size = rd_u32(bytes, OFF_TAG_SIZE) as u64;
    let expected = HEADER_LEN as u64 + data_size + tag_size;
    if expected != bytes.len() as u64 {
        return Err(format!(
            "size mismatch: header declares {data_size} data + {tag_size} tag bytes \
             (total {expected}) but the image is {} bytes",
            bytes.len()
        ));
    }
    // The equality above proves both sizes fit in usize.
    Ok((data_size as usize, tag_size as usize))
}

impl Dc42Image {
    /// Returns true if `bytes` looks like a DiskCopy 4.2 container.
    ///
    /// Checks the magic, the name length, and that the declared data and tag
    /// sizes account for exactly the bytes after the header. Checksums are not
    /// verified — that is [`parse`](Self::parse)'s job — so a corrupted image is
    /// still detected as DiskCopy 4.2 and reported as such rather than as an
    /// unknown format.
    pub(crate) fn detect(bytes: &[u8]) -> bool {
        validate_header(bytes).is_ok()
    }

    /// Parses a DiskCopy 4.2 container, verifying both checksums.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let (data_size, tag_size) = validate_header(bytes).map_err(MfsError::Dc42)?;

        let name_len = bytes[0] as usize;
        let name = bytes[1..1 + name_len].to_vec();
        let data = bytes[HEADER_LEN..HEADER_LEN + data_size].to_vec();
        let tags = bytes[HEADER_LEN + data_size..HEADER_LEN + data_size + tag_size].to_vec();

        let want_data = rd_u32(bytes, OFF_DATA_CKSUM);
        let got_data = checksum(&data);
        if want_data != got_data {
            return Err(MfsError::Dc42(format!(
                "data checksum mismatch: header says {want_data:#010x}, computed {got_data:#010x}"
            )));
        }
        let want_tag = rd_u32(bytes, OFF_TAG_CKSUM);
        let got_tag = tag_checksum(&tags);
        if want_tag != got_tag {
            return Err(MfsError::Dc42(format!(
                "tag checksum mismatch: header says {want_tag:#010x}, computed {got_tag:#010x}"
            )));
        }

        Ok(Dc42Image {
            name,
            data,
            tags,
            disk_format: bytes[OFF_DISK_FORMAT],
            format_byte: bytes[OFF_FORMAT_BYTE],
        })
    }

    /// Serializes the container, recomputing both checksums from the current
    /// data and tags.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_LEN];

        let name_len = self.name.len().min(MAX_NAME_LEN);
        out[0] = name_len as u8;
        out[1..1 + name_len].copy_from_slice(&self.name[..name_len]);

        wr_u32(&mut out, OFF_DATA_SIZE, self.data.len() as u32);
        wr_u32(&mut out, OFF_TAG_SIZE, self.tags.len() as u32);
        wr_u32(&mut out, OFF_DATA_CKSUM, checksum(&self.data));
        wr_u32(&mut out, OFF_TAG_CKSUM, tag_checksum(&self.tags));
        out[OFF_DISK_FORMAT] = self.disk_format;
        out[OFF_FORMAT_BYTE] = self.format_byte;
        wr_u16(&mut out, OFF_MAGIC, MAGIC);

        out.extend_from_slice(&self.data);
        out.extend_from_slice(&self.tags);
        out
    }

    /// Builds a fresh container around `data`, with zeroed tag data.
    ///
    /// DiskCopy 4.2 encodes the geometry as a one-byte format code, so only the
    /// standard floppy sizes can be represented. `name` is truncated to 63
    /// bytes, the longest a Pascal string in the header can hold.
    pub(crate) fn new_blank(name: &[u8], data: Vec<u8>) -> Result<Self> {
        let (disk_format, format_byte) = match data.len() {
            SIZE_400K => (0u8, 0x02u8),
            SIZE_800K => (1u8, 0x22u8),
            other => {
                return Err(MfsError::Dc42(format!(
                    "cannot wrap {other} bytes: DiskCopy 4.2 only defines floppy geometries \
                     ({SIZE_400K} bytes for 400K or {SIZE_800K} bytes for 800K)"
                )));
            }
        };
        let tag_len = (data.len() / 512) * TAG_BYTES_PER_SECTOR;
        Ok(Dc42Image {
            name: name[..name.len().min(MAX_NAME_LEN)].to_vec(),
            data,
            tags: vec![0u8; tag_len],
            disk_format,
            format_byte,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fills a buffer with a cheap, non-repeating pattern so that a single
    /// flipped byte is guaranteed to change the checksum.
    fn pattern(len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..len {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push(x as u8);
        }
        v
    }

    #[test]
    fn checksum_pinned_vectors() {
        // word 1: sum = 0x0001, ror 1 -> 0x8000_0000
        // word 2: sum = 0x8000_0001, ror 1 -> 0xC000_0000
        assert_eq!(checksum(&[0x00, 0x01, 0x00, 0x01]), 0xC000_0000);
        assert_eq!(checksum(&[]), 0);
        assert_eq!(checksum(&[0x00, 0x01]), 0x8000_0000);
        // A lone trailing byte contributes nothing.
        assert_eq!(checksum(&[0xFF]), 0);
    }

    #[test]
    fn tag_checksum_is_zero_without_a_second_sector() {
        assert_eq!(tag_checksum(&[]), 0);
        assert_eq!(tag_checksum(&[0xAB; 12]), 0);
    }

    #[test]
    fn round_trip_400k() {
        let data = pattern(SIZE_400K);
        let img = Dc42Image::new_blank(b"Test Disk", data.clone()).unwrap();
        assert_eq!(img.disk_format, 0);
        assert_eq!(img.format_byte, 0x02);
        assert_eq!(img.tags.len(), (SIZE_400K / 512) * 12);

        let bytes = img.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN + SIZE_400K + 9_600);
        assert!(Dc42Image::detect(&bytes));

        let back = Dc42Image::parse(&bytes).unwrap();
        assert_eq!(back.name, b"Test Disk");
        assert_eq!(back.data, data);
        assert_eq!(back.tags, img.tags);
        assert_eq!(back.disk_format, 0);
        assert_eq!(back.format_byte, 0x02);
        // Serializing the reparsed image reproduces the original bytes.
        assert_eq!(back.to_bytes(), bytes);
    }

    #[test]
    fn round_trip_800k() {
        let img = Dc42Image::new_blank(b"Big", pattern(SIZE_800K)).unwrap();
        assert_eq!((img.disk_format, img.format_byte), (1, 0x22));
        let bytes = img.to_bytes();
        assert!(Dc42Image::detect(&bytes));
        let back = Dc42Image::parse(&bytes).unwrap();
        assert_eq!(back.data.len(), SIZE_800K);
        assert_eq!(back.tags.len(), (SIZE_800K / 512) * 12);
    }

    #[test]
    fn new_blank_truncates_long_names() {
        let long = vec![b'x'; 100];
        let img = Dc42Image::new_blank(&long, vec![0u8; SIZE_400K]).unwrap();
        assert_eq!(img.name.len(), MAX_NAME_LEN);
        let bytes = img.to_bytes();
        assert_eq!(bytes[0], MAX_NAME_LEN as u8);
        assert_eq!(Dc42Image::parse(&bytes).unwrap().name.len(), MAX_NAME_LEN);
    }

    #[test]
    fn new_blank_rejects_non_floppy_sizes() {
        let err = Dc42Image::new_blank(b"odd", vec![0u8; 512]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("floppy geometries"), "{msg}");
    }

    #[test]
    fn detect_rejects_bad_input() {
        let good = Dc42Image::new_blank(b"D", vec![0u8; SIZE_400K])
            .unwrap()
            .to_bytes();

        // Too short.
        assert!(!Dc42Image::detect(&[]));
        assert!(!Dc42Image::detect(&good[..HEADER_LEN - 1]));

        // Bad magic.
        let mut bad_magic = good.clone();
        bad_magic[OFF_MAGIC + 1] = 0x01;
        assert!(!Dc42Image::detect(&bad_magic));

        // Right magic, wrong size arithmetic.
        let mut truncated = good.clone();
        truncated.truncate(good.len() - 1);
        assert!(!Dc42Image::detect(&truncated));

        // Right magic, oversized name length byte.
        let mut long_name = good.clone();
        long_name[0] = 64;
        assert!(!Dc42Image::detect(&long_name));

        // A declared data size that would overflow 32-bit addition.
        let mut overflow = good.clone();
        wr_u32(&mut overflow, OFF_DATA_SIZE, u32::MAX);
        wr_u32(&mut overflow, OFF_TAG_SIZE, u32::MAX);
        assert!(!Dc42Image::detect(&overflow));
    }

    #[test]
    fn parse_rejects_corrupted_data() {
        let mut bytes = Dc42Image::new_blank(b"D", pattern(SIZE_400K))
            .unwrap()
            .to_bytes();
        // The container is still structurally valid...
        bytes[HEADER_LEN + 1000] ^= 0x01;
        assert!(Dc42Image::detect(&bytes));
        // ...but the data checksum no longer matches.
        let msg = Dc42Image::parse(&bytes).unwrap_err().to_string();
        assert!(msg.contains("data checksum mismatch"), "{msg}");
    }

    #[test]
    fn parse_rejects_corrupted_tags() {
        let mut img = Dc42Image::new_blank(b"D", vec![0u8; SIZE_400K]).unwrap();
        img.tags = pattern(9_600);
        let mut bytes = img.to_bytes();
        // Corrupt a tag byte past the skipped first sector.
        bytes[HEADER_LEN + SIZE_400K + 20] ^= 0xFF;
        let msg = Dc42Image::parse(&bytes).unwrap_err().to_string();
        assert!(msg.contains("tag checksum mismatch"), "{msg}");
    }

    #[test]
    fn tag_checksum_skips_first_twelve_bytes() {
        let mut a = Dc42Image::new_blank(b"D", vec![0u8; SIZE_400K]).unwrap();
        a.tags = pattern(9_600);
        let mut b = Dc42Image::new_blank(b"D", vec![0u8; SIZE_400K]).unwrap();
        b.tags = a.tags.clone();
        b.tags[..12].copy_from_slice(&[0xA5; 12]);
        assert_ne!(a.tags[..12], b.tags[..12]);

        let bytes_a = a.to_bytes();
        let bytes_b = b.to_bytes();
        let cksum_a = rd_u32(&bytes_a, OFF_TAG_CKSUM);
        let cksum_b = rd_u32(&bytes_b, OFF_TAG_CKSUM);
        assert_eq!(cksum_a, cksum_b, "first sector's tags must not be checksummed");
        assert_ne!(cksum_a, 0, "the rest of the tags must be checksummed");

        // Both still parse, and the differing tag bytes survive verbatim.
        assert_eq!(Dc42Image::parse(&bytes_a).unwrap().tags, a.tags);
        assert_eq!(Dc42Image::parse(&bytes_b).unwrap().tags, b.tags);
    }

    #[test]
    fn tagless_image_round_trips() {
        let mut img = Dc42Image::new_blank(b"No Tags", pattern(SIZE_400K)).unwrap();
        img.tags.clear();
        let bytes = img.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN + SIZE_400K);
        assert_eq!(rd_u32(&bytes, OFF_TAG_SIZE), 0);
        assert_eq!(rd_u32(&bytes, OFF_TAG_CKSUM), 0);
        assert!(Dc42Image::detect(&bytes));
        let back = Dc42Image::parse(&bytes).unwrap();
        assert!(back.tags.is_empty());
        assert_eq!(back.data, img.data);
    }
}
