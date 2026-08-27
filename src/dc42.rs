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
use binrw::{BinRead, BinWrite, binrw};
use std::io::Cursor;

/// Size of the DiskCopy 4.2 header that precedes the disk data.
pub(crate) const HEADER_LEN: usize = 84;

/// Maximum number of bytes in the header's Pascal-string image name.
const MAX_NAME_LEN: usize = 63;

/// The value every DiskCopy 4.2 header carries in its trailing `magic` field.
const MAGIC: u16 = 0x0100;

/// Where `magic` sits inside the header. Used only to point at the offending
/// bytes in the error message; the layout itself lives in [`Dc42Header`].
const MAGIC_OFFSET: usize = 82;

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

/// The 84-byte DiskCopy 4.2 header, field-for-field as stored on disk.
///
/// The declaration below *is* the layout: fields appear in on-disk order and
/// [`binrw`] reads and writes them big-endian, so the offsets in the table at the
/// top of this module are a consequence of the field order rather than something
/// maintained by hand. Semantic validation is deliberately not expressed as
/// `assert` attributes — it lives in [`validate_header`], which returns a plain
/// `String` so [`Dc42Image::detect`] can run the same checks without minting an
/// error.
#[binrw]
#[brw(big)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dc42Header {
    /// Image name length @0 — the Pascal length byte, at most [`MAX_NAME_LEN`].
    pub name_len: u8,
    /// Image name @1 — the Pascal string's content bytes, zero padded to a
    /// fixed 63; only the first `name_len` of them are meaningful.
    pub name: [u8; MAX_NAME_LEN],
    /// `dataSize` @64 — bytes of disk data following the header.
    pub data_size: u32,
    /// `tagSize` @68 — bytes of tag data following the disk data. May be zero.
    pub tag_size: u32,
    /// `dataChecksum` @72 — [`checksum`] over the disk data.
    pub data_cksum: u32,
    /// `tagChecksum` @76 — [`tag_checksum`] over the tag data.
    pub tag_cksum: u32,
    /// `diskFormat` @80 — 0 = 400K, 1 = 800K, 2 = 720K, 3 = 1440K.
    pub disk_format: u8,
    /// `formatByte` @81 — 0x02 = 400K, 0x22 = >400K Mac, 0x24 = 720K/1440K.
    pub format_byte: u8,
    /// `magic` @82 — always [`MAGIC`].
    ///
    /// A plain field rather than a `#[brw(magic = ...)]` directive: that
    /// directive matches a signature at the *start* of a struct, and DiskCopy
    /// puts its magic last. The value is checked in [`validate_header`].
    pub magic: u16,
}

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
/// [`Dc42Image::parse`], returning the decoded header along with the data and
/// tag sizes as `usize`.
///
/// The size arithmetic is done in `u64` so that a hostile header claiming a
/// 4 GB data size cannot wrap around into a plausible-looking total.
fn validate_header(bytes: &[u8]) -> std::result::Result<(Dc42Header, usize, usize), String> {
    if bytes.len() < HEADER_LEN {
        return Err(format!(
            "image is {} bytes, shorter than the {HEADER_LEN}-byte header",
            bytes.len()
        ));
    }
    // The length check above makes this read infallible; anything past the
    // header belongs to the disk and tag data.
    let header = Dc42Header::read(&mut Cursor::new(&bytes[..HEADER_LEN]))
        .map_err(|e| format!("cannot read the DiskCopy header: {e}"))?;

    if header.magic != MAGIC {
        return Err(format!(
            "bad magic {:#04x}{:02x} at offset {MAGIC_OFFSET} (expected {MAGIC:#06x})",
            (header.magic >> 8) as u8,
            header.magic as u8
        ));
    }
    let name_len = header.name_len as usize;
    if name_len > MAX_NAME_LEN {
        return Err(format!(
            "image name length {name_len} exceeds the {MAX_NAME_LEN}-byte maximum"
        ));
    }
    let data_size = header.data_size as u64;
    let tag_size = header.tag_size as u64;
    let expected = HEADER_LEN as u64 + data_size + tag_size;
    if expected != bytes.len() as u64 {
        return Err(format!(
            "size mismatch: header declares {data_size} data + {tag_size} tag bytes \
             (total {expected}) but the image is {} bytes",
            bytes.len()
        ));
    }
    // The equality above proves both sizes fit in usize.
    Ok((header, data_size as usize, tag_size as usize))
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
        let (header, data_size, tag_size) = validate_header(bytes).map_err(MfsError::Dc42)?;

        // Only the Pascal string's content is kept; the zero padding after it is
        // regenerated on write rather than preserved.
        let name = header.name[..header.name_len as usize].to_vec();
        let data = bytes[HEADER_LEN..HEADER_LEN + data_size].to_vec();
        let tags = bytes[HEADER_LEN + data_size..HEADER_LEN + data_size + tag_size].to_vec();

        let want_data = header.data_cksum;
        let got_data = checksum(&data);
        if want_data != got_data {
            return Err(MfsError::Dc42(format!(
                "data checksum mismatch: header says {want_data:#010x}, computed {got_data:#010x}"
            )));
        }
        let want_tag = header.tag_cksum;
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
            disk_format: header.disk_format,
            format_byte: header.format_byte,
        })
    }

    /// Serializes the container, recomputing both checksums from the current
    /// data and tags.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        // Names longer than the Pascal string can hold are truncated; the rest
        // of the fixed-width field is zero padding.
        let name_len = self.name.len().min(MAX_NAME_LEN);
        let mut name = [0u8; MAX_NAME_LEN];
        name[..name_len].copy_from_slice(&self.name[..name_len]);

        let header = Dc42Header {
            name_len: name_len as u8,
            name,
            data_size: self.data.len() as u32,
            tag_size: self.tags.len() as u32,
            data_cksum: checksum(&self.data),
            tag_cksum: tag_checksum(&self.tags),
            disk_format: self.disk_format,
            format_byte: self.format_byte,
            magic: MAGIC,
        };

        let mut out = Vec::with_capacity(HEADER_LEN + self.data.len() + self.tags.len());
        header
            .write(&mut Cursor::new(&mut out))
            .expect("the header is fixed-size and the sink is a Vec, so this write cannot fail");
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

    /// Decodes the header of a serialized image, so tests can inspect header
    /// fields without repeating the offsets [`Dc42Header`] already declares.
    fn header_of(bytes: &[u8]) -> Dc42Header {
        Dc42Header::read(&mut Cursor::new(&bytes[..HEADER_LEN])).unwrap()
    }

    /// Rewrites the header of a serialized image in place.
    fn patch_header(bytes: &mut [u8], f: impl FnOnce(&mut Dc42Header)) {
        let mut header = header_of(bytes);
        f(&mut header);
        header
            .write(&mut Cursor::new(&mut bytes[..HEADER_LEN]))
            .unwrap();
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
        bad_magic[MAGIC_OFFSET + 1] = 0x01;
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
        patch_header(&mut overflow, |h| {
            h.data_size = u32::MAX;
            h.tag_size = u32::MAX;
        });
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
        let cksum_a = header_of(&bytes_a).tag_cksum;
        let cksum_b = header_of(&bytes_b).tag_cksum;
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
        let header = header_of(&bytes);
        assert_eq!(header.tag_size, 0);
        assert_eq!(header.tag_cksum, 0);
        assert!(Dc42Image::detect(&bytes));
        let back = Dc42Image::parse(&bytes).unwrap();
        assert!(back.tags.is_empty());
        assert_eq!(back.data, img.data);
    }
}
