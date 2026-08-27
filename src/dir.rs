//! File directory entries.
//!
//! The directory occupies `drDirLen` consecutive 512-byte sectors starting at
//! `drDirSt`. Entries are variable length and even-padded, and they never span
//! a sector boundary: within a sector, entries are packed from offset 0 and the
//! first flags byte with bit 7 clear terminates that sector's entries (the rest
//! is padding). Parsing then continues at the start of the next sector.
//!
//! Entry layout (all big-endian):
//!
//! | off | field           | size |
//! |-----|-----------------|------|
//! | 0   | `flFlags`       | 1    |
//! | 1   | `flVersion`     | 1    |
//! | 2   | `flTyp`         | 4    |
//! | 6   | `flCr`          | 4    |
//! | 10  | `flFndrFlags`   | 2    |
//! | 12  | `flPos`         | 4    |
//! | 16  | `flFldrNum`     | 2    |
//! | 18  | `flFNum`        | 4    |
//! | 22  | `flDFStBlk`     | 2    |
//! | 24  | `flDFLogLen`    | 4    |
//! | 28  | `flDFAllocLen`  | 4    |
//! | 32  | `flRFStBlk`     | 2    |
//! | 34  | `flRFLogLen`    | 4    |
//! | 38  | `flRFAllocLen`  | 4    |
//! | 42  | `flCrDat`       | 4    |
//! | 46  | `flMdDat`       | 4    |
//! | 50  | name length     | 1    |
//! | 51  | name bytes      | n    |

use crate::error::{MfsError, Result};
use crate::util::{rd_u16, rd_u32, wr_u16, wr_u32};

/// Logical sector size; directory entries are packed into sectors of this size.
pub(crate) const SECTOR: usize = 512;

/// Fixed part of a directory entry: everything up to and including the name
/// length byte.
const FIXED_LEN: usize = 51;

/// `flFlags` bit 7 — the entry is in use.
const FLAG_IN_USE: u8 = 0x80;
/// `flFlags` bit 0 — the file is locked.
const FLAG_LOCKED: u8 = 0x01;

/// A directory entry exactly as stored on disk.
///
/// Every field — including ones this crate never interprets (`version`, `pos`,
/// `fldr_num`) — is kept verbatim so that open→save round-trips byte-identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    /// `flFlags` @0 — bit 7 in use, bit 0 locked.
    pub flags: u8,
    /// `flVersion` @1 — always zero in practice. Stored verbatim.
    pub version: u8,
    /// `flTyp` @2 — Finder type code.
    pub type_code: [u8; 4],
    /// `flCr` @6 — Finder creator code.
    pub creator: [u8; 4],
    /// `flFndrFlags` @10 — Finder flags.
    pub fndr_flags: u16,
    /// `flPos` @12 — Finder icon position. Stored verbatim.
    pub pos: u32,
    /// `flFldrNum` @16 — Finder folder number. Stored verbatim.
    pub fldr_num: u16,
    /// `flFNum` @18 — file number.
    pub fnum: u32,
    /// `flDFStBlk` @22 — first allocation block of the data fork.
    pub df_st_blk: u16,
    /// `flDFLogLen` @24 — data fork logical length in bytes.
    pub df_log_len: u32,
    /// `flDFAllocLen` @28 — data fork allocated length in bytes.
    pub df_alloc_len: u32,
    /// `flRFStBlk` @32 — first allocation block of the resource fork.
    pub rf_st_blk: u16,
    /// `flRFLogLen` @34 — resource fork logical length in bytes.
    pub rf_log_len: u32,
    /// `flRFAllocLen` @38 — resource fork allocated length in bytes.
    pub rf_alloc_len: u32,
    /// `flCrDat` @42 — creation date (1904 epoch).
    pub cr_dat: u32,
    /// `flMdDat` @46 — last modification date (1904 epoch).
    pub md_dat: u32,
    /// Raw MacRoman name bytes @51, without the leading Pascal length byte
    /// (@50, derived from `name.len()` when writing).
    pub name: Vec<u8>,
}

impl RawEntry {
    /// Whether `flFlags` bit 7 is set.
    pub(crate) fn in_use(&self) -> bool {
        self.flags & FLAG_IN_USE != 0
    }

    /// Whether `flFlags` bit 0 is set.
    pub(crate) fn locked(&self) -> bool {
        self.flags & FLAG_LOCKED != 0
    }

    /// Number of bytes this entry occupies on disk, including even padding.
    pub(crate) fn entry_len(&self) -> usize {
        entry_len_for(self.name.len())
    }
}

/// On-disk length of a directory entry whose name is `name_len` bytes, rounded
/// up to an even boundary.
pub(crate) fn entry_len_for(name_len: usize) -> usize {
    (FIXED_LEN + name_len + 1) & !1
}

/// Parse every in-use entry in the file directory region.
///
/// `region` must cover whole sectors (`drDirLen * 512` bytes).
pub(crate) fn parse_directory(region: &[u8]) -> Result<Vec<RawEntry>> {
    if !region.len().is_multiple_of(SECTOR) {
        return Err(MfsError::CorruptVolume(format!(
            "directory region is {} bytes, not a multiple of {SECTOR}",
            region.len()
        )));
    }

    let mut entries = Vec::new();
    for sector in region.chunks_exact(SECTOR) {
        let mut pos = 0usize;
        // An entry needs at least its fixed part; a flags byte with bit 7 clear
        // terminates this sector.
        while pos + FIXED_LEN <= SECTOR && sector[pos] & FLAG_IN_USE != 0 {
            let name_len = sector[pos + 50] as usize;
            let len = entry_len_for(name_len);
            if pos + len > SECTOR {
                return Err(MfsError::CorruptVolume(
                    "directory entry overruns sector".to_string(),
                ));
            }
            let e = &sector[pos..pos + len];
            entries.push(RawEntry {
                flags: e[0],
                version: e[1],
                type_code: [e[2], e[3], e[4], e[5]],
                creator: [e[6], e[7], e[8], e[9]],
                fndr_flags: rd_u16(e, 10),
                pos: rd_u32(e, 12),
                fldr_num: rd_u16(e, 16),
                fnum: rd_u32(e, 18),
                df_st_blk: rd_u16(e, 22),
                df_log_len: rd_u32(e, 24),
                df_alloc_len: rd_u32(e, 28),
                rf_st_blk: rd_u16(e, 32),
                rf_log_len: rd_u32(e, 34),
                rf_alloc_len: rd_u32(e, 38),
                cr_dat: rd_u32(e, 42),
                md_dat: rd_u32(e, 46),
                name: e[51..51 + name_len].to_vec(),
            });
            pos += len;
        }
    }
    Ok(entries)
}

/// Compute where each entry lands when first-fit packed into `n_sectors`
/// sectors, as absolute byte offsets from the start of the directory region.
///
/// Returns `None` if the entries do not fit (or if any name is unrepresentable).
fn plan(entries: &[RawEntry], n_sectors: usize) -> Option<Vec<usize>> {
    let mut offsets = Vec::with_capacity(entries.len());
    let mut sector = 0usize;
    let mut off = 0usize;
    for e in entries {
        if e.name.len() > u8::MAX as usize {
            return None;
        }
        let len = e.entry_len();
        debug_assert!(len <= SECTOR);
        if off + len > SECTOR {
            sector += 1;
            off = 0;
        }
        if sector >= n_sectors {
            return None;
        }
        offsets.push(sector * SECTOR + off);
        off += len;
    }
    Some(offsets)
}

/// Whether `entries` can be packed into `n_sectors` directory sectors.
///
/// A dry run of the exact algorithm [`write_directory`] uses, so callers can
/// fail with `DirectoryFull` up front rather than at save time.
pub(crate) fn fits(entries: &[RawEntry], n_sectors: usize) -> bool {
    plan(entries, n_sectors).is_some()
}

/// Rebuild the whole directory region from `entries`.
///
/// The region is zeroed first, so the zero flags byte after the last entry in
/// each sector acts as that sector's terminator for free.
pub(crate) fn write_directory(entries: &[RawEntry], region: &mut [u8]) -> Result<()> {
    if !region.len().is_multiple_of(SECTOR) {
        return Err(MfsError::CorruptVolume(format!(
            "directory region is {} bytes, not a multiple of {SECTOR}",
            region.len()
        )));
    }
    let n_sectors = region.len() / SECTOR;
    let offsets = plan(entries, n_sectors).ok_or(MfsError::DirectoryFull)?;

    region.fill(0);
    for (e, &start) in entries.iter().zip(offsets.iter()) {
        let len = e.entry_len();
        let out = &mut region[start..start + len];
        // An entry is only reachable by the parser with bit 7 set.
        out[0] = e.flags | FLAG_IN_USE;
        out[1] = e.version;
        out[2..6].copy_from_slice(&e.type_code);
        out[6..10].copy_from_slice(&e.creator);
        wr_u16(out, 10, e.fndr_flags);
        wr_u32(out, 12, e.pos);
        wr_u16(out, 16, e.fldr_num);
        wr_u32(out, 18, e.fnum);
        wr_u16(out, 22, e.df_st_blk);
        wr_u32(out, 24, e.df_log_len);
        wr_u32(out, 28, e.df_alloc_len);
        wr_u16(out, 32, e.rf_st_blk);
        wr_u32(out, 34, e.rf_log_len);
        wr_u32(out, 38, e.rf_alloc_len);
        wr_u32(out, 42, e.cr_dat);
        wr_u32(out, 46, e.md_dat);
        out[50] = e.name.len() as u8;
        out[51..51 + e.name.len()].copy_from_slice(&e.name);
        // Bytes 51+name_len..len (at most one) stay zero from the fill above.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fnum: u32, name: &str) -> RawEntry {
        RawEntry {
            flags: FLAG_IN_USE,
            version: 0,
            type_code: *b"TEXT",
            creator: *b"MACA",
            fndr_flags: 0x0100,
            pos: 0,
            fldr_num: 0,
            fnum,
            df_st_blk: 2,
            df_log_len: 1234,
            df_alloc_len: 2048,
            rf_st_blk: 0,
            rf_log_len: 0,
            rf_alloc_len: 0,
            cr_dat: 0xA1B2_C3D4,
            md_dat: 0xA1B2_C3E8,
            name: name.as_bytes().to_vec(),
        }
    }

    /// An entry with every field distinct, to catch swapped offsets.
    fn rich_entry(name: &str) -> RawEntry {
        RawEntry {
            flags: FLAG_IN_USE | FLAG_LOCKED,
            version: 0x5A,
            type_code: *b"APPL",
            creator: *b"MPS ",
            fndr_flags: 0x1234,
            pos: 0x1122_3344,
            fldr_num: 0x5566,
            fnum: 0x7788_99AA,
            df_st_blk: 0x00BC,
            df_log_len: 0x0102_0304,
            df_alloc_len: 0x0506_0708,
            rf_st_blk: 0x00DE,
            rf_log_len: 0x090A_0B0C,
            rf_alloc_len: 0x0D0E_0F10,
            cr_dat: 0x1112_1314,
            md_dat: 0x1516_1718,
            name: name.as_bytes().to_vec(),
        }
    }

    #[test]
    fn entry_len_is_even_padded() {
        assert_eq!(entry_len_for(0), 52);
        assert_eq!(entry_len_for(1), 52);
        assert_eq!(entry_len_for(2), 54);
        assert_eq!(entry_len_for(3), 54);
        assert_eq!(entry_len_for(4), 56);
        assert_eq!(entry_len_for(5), 56);
        // An entry always has room for its fixed part plus the name.
        for n in 0..=255usize {
            assert!(entry_len_for(n) >= FIXED_LEN + n);
            assert_eq!(entry_len_for(n) % 2, 0);
        }
    }

    #[test]
    fn flag_accessors() {
        let mut e = entry(1, "x");
        assert!(e.in_use());
        assert!(!e.locked());
        e.flags |= FLAG_LOCKED;
        assert!(e.locked());
        e.flags = 0;
        assert!(!e.in_use());
    }

    #[test]
    fn round_trips_mixed_name_lengths() {
        let entries: Vec<RawEntry> = [
            "A",           // odd
            "AB",          // even
            "System",      // even
            "Finder",      // even
            "MacPaint!",   // odd
            "a very long file name of thirty-nine ch", // odd, 39
            "a very long file name of forty chars ok!", // even, 40
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| rich_entry(n).tap_fnum(i as u32 + 1))
        .collect();

        let mut region = vec![0u8; SECTOR * 4];
        write_directory(&entries, &mut region).unwrap();
        let back = parse_directory(&region).unwrap();
        assert_eq!(back, entries);
    }

    // Small helper so the map above stays readable.
    trait TapFnum {
        fn tap_fnum(self, n: u32) -> RawEntry;
    }
    impl TapFnum for RawEntry {
        fn tap_fnum(mut self, n: u32) -> RawEntry {
            self.fnum = n;
            self
        }
    }

    #[test]
    fn empty_directory_round_trips() {
        let mut region = vec![0xFFu8; SECTOR * 2];
        write_directory(&[], &mut region).unwrap();
        assert!(region.iter().all(|&b| b == 0));
        assert_eq!(parse_directory(&region).unwrap(), Vec::<RawEntry>::new());
    }

    #[test]
    fn entries_exactly_filling_a_sector() {
        // 512 = 52 * 8 + 96; use names sized so the total hits 512 exactly:
        // seven 52-byte entries (name len 0 or 1) = 364, plus one 148-byte
        // entry (name len 96 -> 51 + 96 + 1 = 148).
        let mut entries: Vec<RawEntry> = (0..7).map(|i| entry(i, "x")).collect();
        entries.push(entry(7, &"n".repeat(96)));
        let total: usize = entries.iter().map(|e| e.entry_len()).sum();
        assert_eq!(total, SECTOR);

        assert!(fits(&entries, 1));
        let mut region = vec![0u8; SECTOR];
        write_directory(&entries, &mut region).unwrap();
        // Every byte is used: no terminator needed, the sector simply ends.
        assert_eq!(parse_directory(&region).unwrap(), entries);
    }

    #[test]
    fn oversized_entry_starts_a_new_sector() {
        // Nine 52-byte entries fill 468 bytes; 44 remain, too few for a 52-byte
        // entry, so the tenth must move to sector 1.
        let mut entries: Vec<RawEntry> = (0..9).map(|i| entry(i, "x")).collect();
        entries.push(entry(9, "spills"));
        let mut region = vec![0u8; SECTOR * 2];
        write_directory(&entries, &mut region).unwrap();

        assert_eq!(region[9 * 52], 0, "sector 0 remainder must be zero padding");
        assert_ne!(region[SECTOR], 0, "the tenth entry belongs in sector 1");
        assert_eq!(region[SECTOR + 50] as usize, "spills".len());
        assert_eq!(parse_directory(&region).unwrap(), entries);
    }

    #[test]
    fn directory_full_at_exact_capacity() {
        // Each entry is 52 bytes; nine fit per 512-byte sector (468 bytes used).
        let make = |n: u32| -> Vec<RawEntry> { (0..n).map(|i| entry(i, "x")).collect() };

        let nine = make(9);
        assert!(fits(&nine, 1));
        let mut region = vec![0u8; SECTOR];
        assert!(write_directory(&nine, &mut region).is_ok());
        assert_eq!(parse_directory(&region).unwrap().len(), 9);

        let ten = make(10);
        assert!(!fits(&ten, 1));
        let mut region = vec![0u8; SECTOR];
        assert!(matches!(
            write_directory(&ten, &mut region),
            Err(MfsError::DirectoryFull)
        ));

        // Two sectors hold exactly eighteen.
        assert!(fits(&make(18), 2));
        assert!(!fits(&make(19), 2));
        // Zero sectors hold nothing but the empty list.
        assert!(fits(&[], 0));
        assert!(!fits(&make(1), 0));
    }

    #[test]
    fn failed_write_leaves_the_region_untouched() {
        let mut region = vec![0xAAu8; SECTOR];
        let entries: Vec<RawEntry> = (0..10).map(|i| entry(i, "x")).collect();
        assert!(write_directory(&entries, &mut region).is_err());
        assert!(region.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn terminator_stops_parsing_before_garbage() {
        let mut region = vec![0u8; SECTOR * 2];
        // One real entry at the start of sector 0.
        write_directory(&[rich_entry("Real")], &mut region).unwrap();
        let one_len = entry_len_for(4);
        // Zero terminator (already there), then garbage that would look like an
        // entry if the parser kept scanning.
        for (i, b) in region[one_len + 1..SECTOR].iter_mut().enumerate() {
            *b = if i % 3 == 0 { 0xFF } else { i as u8 };
        }
        // Sector 1 has a valid entry, which must still be found.
        let mut sector1 = vec![0u8; SECTOR];
        write_directory(&[rich_entry("Second")], &mut sector1).unwrap();
        region[SECTOR..].copy_from_slice(&sector1);

        let back = parse_directory(&region).unwrap();
        assert_eq!(back, vec![rich_entry("Real"), rich_entry("Second")]);
    }

    #[test]
    fn rejects_region_that_is_not_whole_sectors() {
        let mut region = vec![0u8; SECTOR + 1];
        assert!(matches!(
            parse_directory(&region),
            Err(MfsError::CorruptVolume(_))
        ));
        assert!(matches!(
            write_directory(&[], &mut region),
            Err(MfsError::CorruptVolume(_))
        ));
    }

    #[test]
    fn rejects_entry_overrunning_its_sector() {
        let mut region = vec![0u8; SECTOR];
        // Eight 52-byte entries end at 416; there is room for the parser to read
        // a ninth header there, so hand-plant one whose name runs past the end.
        let entries: Vec<RawEntry> = (0..8).map(|i| entry(i, "x")).collect();
        write_directory(&entries, &mut region).unwrap();
        let pos = 8 * 52; // 416; 416 + 51 <= 512, so the parser will look
        region[pos] = FLAG_IN_USE;
        region[pos + 50] = 200; // entry_len 252 -> 416 + 252 overruns the sector
        assert!(matches!(
            parse_directory(&region),
            Err(MfsError::CorruptVolume(_))
        ));
    }

    #[test]
    fn parser_ignores_a_trailing_stub_too_small_for_an_entry() {
        // 468 bytes used leaves 44 < 51: even a set flags byte there is padding
        // the parser must not try to decode.
        let entries: Vec<RawEntry> = (0..9).map(|i| entry(i, "x")).collect();
        let mut region = vec![0u8; SECTOR];
        write_directory(&entries, &mut region).unwrap();
        region[9 * 52] = 0xFF;
        region[SECTOR - 1] = 0xFF;
        assert_eq!(parse_directory(&region).unwrap(), entries);
    }
}
