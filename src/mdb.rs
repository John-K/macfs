//! Master Directory Block (MDB) parse/serialize.
//!
//! The MDB occupies the first 64 bytes of logical sector 2 (volume byte offset
//! 1024). All fields are big-endian. The 12-bit allocation block map follows
//! immediately at byte 64 of the same region and is handled by
//! [`crate::blockmap`].

use crate::error::{MfsError, Result};
use crate::util::{rd_u16, rd_u32, wr_u16, wr_u32};

/// `drSigWord` — the value that marks a volume as MFS.
pub(crate) const MFS_SIGNATURE: u16 = 0xD2D7;

/// `drSigWord` of an HFS volume ("BD"), whose MDB sits at the same offset.
/// Recognized only so HFS images can be reported as unsupported.
pub(crate) const HFS_SIGNATURE: u16 = 0x4244;

/// Serialized size of the MDB proper (the allocation block map starts right after).
pub(crate) const MDB_LEN: usize = 64;

/// The Master Directory Block, field-for-field as stored on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mdb {
    /// `drSigWord` @0 — always [`MFS_SIGNATURE`].
    pub sig_word: u16,
    /// `drCrDate` @2 — volume creation date (1904 epoch).
    pub cr_date: u32,
    /// `drLsMod` @6 — last modification date (1904 epoch).
    pub ls_mod: u32,
    /// `drAtrb` @10 — bit 15 hardware lock, bit 7 software lock. Stored verbatim.
    pub atrb: u16,
    /// `drNmFls` @12 — number of files in the directory.
    pub nm_fls: u16,
    /// `drDirSt` @14 — first sector of the file directory.
    pub dir_st: u16,
    /// `drDirLen` @16 — length of the file directory, in sectors.
    pub dir_len: u16,
    /// `drNmAlBlks` @18 — number of allocation blocks on the volume.
    pub nm_al_blks: u16,
    /// `drAlBlkSiz` @20 — allocation block size in bytes; a multiple of 512.
    pub al_blk_siz: u32,
    /// `drClpSiz` @24 — default clump size in bytes. Stored verbatim.
    pub clp_siz: u32,
    /// `drAlBlSt` @28 — first sector of the allocation block area.
    pub al_bl_st: u16,
    /// `drNxtFNum` @30 — next unused file number.
    pub nxt_fnum: u32,
    /// `drFreeBks` @34 — number of unused allocation blocks.
    pub free_bks: u16,
    /// `drVN` @36 — volume name as a Pascal string: length byte plus up to 27
    /// MacRoman bytes. Stored verbatim (including trailing garbage) so that
    /// open→save round-trips byte-identically.
    pub name_raw: [u8; 28],
}

impl Mdb {
    /// Parse an MDB from a region whose byte 0 is the MDB's first byte
    /// (volume byte offset 1024).
    ///
    /// Performs the sanity checks that every later stage relies on: signature,
    /// allocation block size, and the relative ordering of the directory and
    /// allocation block areas.
    pub(crate) fn parse(region: &[u8]) -> Result<Mdb> {
        if region.len() < MDB_LEN {
            return Err(MfsError::CorruptVolume(format!(
                "MDB region is {} bytes, need at least {MDB_LEN}",
                region.len()
            )));
        }

        let sig_word = rd_u16(region, 0);
        if sig_word == HFS_SIGNATURE {
            return Err(MfsError::UnsupportedHfs);
        }
        if sig_word != MFS_SIGNATURE {
            return Err(MfsError::BadSignature { found: sig_word });
        }

        let al_blk_siz = rd_u32(region, 20);
        if al_blk_siz == 0 || !al_blk_siz.is_multiple_of(512) {
            return Err(MfsError::CorruptVolume(format!(
                "drAlBlkSiz is {al_blk_siz}, expected a nonzero multiple of 512"
            )));
        }

        let dir_st = rd_u16(region, 14);
        if dir_st < 3 {
            return Err(MfsError::CorruptVolume(format!(
                "drDirSt is {dir_st}, expected at least 3 (boot blocks + MDB)"
            )));
        }

        let dir_len = rd_u16(region, 16);
        if dir_len < 1 {
            return Err(MfsError::CorruptVolume(
                "drDirLen is 0, expected at least 1 sector".to_string(),
            ));
        }

        let al_bl_st = rd_u16(region, 28);
        // dir_st and dir_len are u16; widen so the sum cannot wrap.
        let dir_end = dir_st as u32 + dir_len as u32;
        if (al_bl_st as u32) < dir_end {
            return Err(MfsError::CorruptVolume(format!(
                "drAlBlSt is {al_bl_st}, expected at least {dir_end} \
                 (drDirSt {dir_st} + drDirLen {dir_len})"
            )));
        }

        let mut name_raw = [0u8; 28];
        name_raw.copy_from_slice(&region[36..64]);
        if name_raw[0] > 27 {
            return Err(MfsError::CorruptVolume(format!(
                "volume name length byte is {}, maximum is 27",
                name_raw[0]
            )));
        }

        Ok(Mdb {
            sig_word,
            cr_date: rd_u32(region, 2),
            ls_mod: rd_u32(region, 6),
            atrb: rd_u16(region, 10),
            nm_fls: rd_u16(region, 12),
            dir_st,
            dir_len,
            nm_al_blks: rd_u16(region, 18),
            al_blk_siz,
            clp_siz: rd_u32(region, 24),
            al_bl_st,
            nxt_fnum: rd_u32(region, 30),
            free_bks: rd_u16(region, 34),
            name_raw,
        })
    }

    /// Serialize the MDB into the first 64 bytes of `out` — the exact inverse
    /// of [`Mdb::parse`].
    ///
    /// # Panics
    /// Panics if `out` is shorter than 64 bytes (an internal invariant: the
    /// caller owns the whole-image buffer).
    pub(crate) fn write_to(&self, out: &mut [u8]) {
        assert!(
            out.len() >= MDB_LEN,
            "MDB output region must be at least {MDB_LEN} bytes"
        );
        wr_u16(out, 0, self.sig_word);
        wr_u32(out, 2, self.cr_date);
        wr_u32(out, 6, self.ls_mod);
        wr_u16(out, 10, self.atrb);
        wr_u16(out, 12, self.nm_fls);
        wr_u16(out, 14, self.dir_st);
        wr_u16(out, 16, self.dir_len);
        wr_u16(out, 18, self.nm_al_blks);
        wr_u32(out, 20, self.al_blk_siz);
        wr_u32(out, 24, self.clp_siz);
        wr_u16(out, 28, self.al_bl_st);
        wr_u32(out, 30, self.nxt_fnum);
        wr_u16(out, 34, self.free_bks);
        out[36..64].copy_from_slice(&self.name_raw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built, plausible 400K MFS MDB.
    fn sample_bytes() -> [u8; 64] {
        let mut b = [0u8; 64];
        b[0..2].copy_from_slice(&0xD2D7u16.to_be_bytes()); // drSigWord
        b[2..6].copy_from_slice(&0xA1B2_C3D4u32.to_be_bytes()); // drCrDate
        b[6..10].copy_from_slice(&0xA1B2_C3E8u32.to_be_bytes()); // drLsMod
        b[10..12].copy_from_slice(&0x0080u16.to_be_bytes()); // drAtrb (SW locked)
        b[12..14].copy_from_slice(&7u16.to_be_bytes()); // drNmFls
        b[14..16].copy_from_slice(&4u16.to_be_bytes()); // drDirSt
        b[16..18].copy_from_slice(&12u16.to_be_bytes()); // drDirLen
        b[18..20].copy_from_slice(&391u16.to_be_bytes()); // drNmAlBlks
        b[20..24].copy_from_slice(&1024u32.to_be_bytes()); // drAlBlkSiz
        b[24..28].copy_from_slice(&8192u32.to_be_bytes()); // drClpSiz
        b[28..30].copy_from_slice(&16u16.to_be_bytes()); // drAlBlSt
        b[30..34].copy_from_slice(&23u32.to_be_bytes()); // drNxtFNum
        b[34..36].copy_from_slice(&300u16.to_be_bytes()); // drFreeBks
        // drVN: "Untitled" plus deliberate trailing garbage that must survive.
        b[36] = 8;
        b[37..45].copy_from_slice(b"Untitled");
        b[45] = 0xEE;
        b[63] = 0x11;
        b
    }

    #[test]
    fn parses_every_field() {
        let m = Mdb::parse(&sample_bytes()).unwrap();
        assert_eq!(m.sig_word, MFS_SIGNATURE);
        assert_eq!(m.cr_date, 0xA1B2_C3D4);
        assert_eq!(m.ls_mod, 0xA1B2_C3E8);
        assert_eq!(m.atrb, 0x0080);
        assert_eq!(m.nm_fls, 7);
        assert_eq!(m.dir_st, 4);
        assert_eq!(m.dir_len, 12);
        assert_eq!(m.nm_al_blks, 391);
        assert_eq!(m.al_blk_siz, 1024);
        assert_eq!(m.clp_siz, 8192);
        assert_eq!(m.al_bl_st, 16);
        assert_eq!(m.nxt_fnum, 23);
        assert_eq!(m.free_bks, 300);
        assert_eq!(m.name_raw[0], 8);
        assert_eq!(&m.name_raw[1..9], b"Untitled");
        assert_eq!(m.name_raw[9], 0xEE);
        assert_eq!(m.name_raw[27], 0x11);
    }

    #[test]
    fn write_to_round_trips_byte_identically() {
        let bytes = sample_bytes();
        let m = Mdb::parse(&bytes).unwrap();
        let mut out = [0xAAu8; 64];
        m.write_to(&mut out);
        assert_eq!(out, bytes);
        // ...and re-parsing yields an identical struct.
        assert_eq!(Mdb::parse(&out).unwrap(), m);
    }

    #[test]
    fn parse_accepts_region_longer_than_64_bytes() {
        let mut buf = vec![0u8; 512];
        buf[..64].copy_from_slice(&sample_bytes());
        buf[64] = 0xFF; // allocation block map bytes; must be ignored
        assert!(Mdb::parse(&buf).is_ok());
    }

    #[test]
    fn rejects_short_region() {
        let bytes = sample_bytes();
        assert!(matches!(
            Mdb::parse(&bytes[..63]),
            Err(MfsError::CorruptVolume(_))
        ));
    }

    #[test]
    fn rejects_bad_signature() {
        let mut b = sample_bytes();
        b[0..2].copy_from_slice(&0xBEEFu16.to_be_bytes());
        match Mdb::parse(&b) {
            Err(MfsError::BadSignature { found }) => assert_eq!(found, 0xBEEF),
            other => panic!("expected BadSignature, got {other:?}"),
        }
    }

    #[test]
    fn hfs_signature_is_unsupported() {
        let mut b = sample_bytes();
        b[0..2].copy_from_slice(&HFS_SIGNATURE.to_be_bytes());
        assert!(matches!(Mdb::parse(&b), Err(MfsError::UnsupportedHfs)));
    }

    #[test]
    fn rejects_bad_alloc_block_size() {
        for bad in [0u32, 1, 511, 513, 1000] {
            let mut b = sample_bytes();
            b[20..24].copy_from_slice(&bad.to_be_bytes());
            assert!(
                matches!(Mdb::parse(&b), Err(MfsError::CorruptVolume(_))),
                "drAlBlkSiz {bad} should be rejected"
            );
        }
        // 512 and 2048 are legal.
        for good in [512u32, 1024, 2048] {
            let mut b = sample_bytes();
            b[20..24].copy_from_slice(&good.to_be_bytes());
            assert!(Mdb::parse(&b).is_ok(), "drAlBlkSiz {good} should be accepted");
        }
    }

    #[test]
    fn rejects_bad_directory_geometry() {
        // drDirSt below 3 (would overlap the boot blocks / MDB).
        let mut b = sample_bytes();
        b[14..16].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(Mdb::parse(&b), Err(MfsError::CorruptVolume(_))));

        // Zero-length directory.
        let mut b = sample_bytes();
        b[16..18].copy_from_slice(&0u16.to_be_bytes());
        assert!(matches!(Mdb::parse(&b), Err(MfsError::CorruptVolume(_))));

        // Allocation area starting inside the directory.
        let mut b = sample_bytes();
        b[28..30].copy_from_slice(&15u16.to_be_bytes()); // 4 + 12 == 16
        assert!(matches!(Mdb::parse(&b), Err(MfsError::CorruptVolume(_))));

        // Exactly abutting is fine.
        let mut b = sample_bytes();
        b[28..30].copy_from_slice(&16u16.to_be_bytes());
        assert!(Mdb::parse(&b).is_ok());
    }

    #[test]
    fn rejects_overlong_volume_name() {
        let mut b = sample_bytes();
        b[36] = 28;
        assert!(matches!(Mdb::parse(&b), Err(MfsError::CorruptVolume(_))));
        b[36] = 27;
        assert!(Mdb::parse(&b).is_ok());
    }
}
