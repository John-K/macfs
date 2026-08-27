//! Volume formatting.
//!
//! The geometry here is not invented: it is the algorithm that reproduces, bit
//! for bit, the layout Apple's own formatter wrote onto the blank 400K and 800K
//! MFS floppies shipped with Mini vMac (`tests/images/gryphel-mfs*.image`).
//!
//! ```text
//!               400K (409,600 B)   800K (819,200 B)
//! drAlBlkSiz          1024               2048
//! drDirSt                4                  4
//! drDirLen              12                 26
//! drAlBlSt              16                 30
//! drNmAlBlks           391                392
//! drClpSiz            8192              16384
//! ```
//!
//! Three empirical rules fall out of those two data points, and both geometries
//! drop out of them exactly:
//!
//! 1. The last two sectors of the disk are left out of the allocation area.
//! 2. There is roughly one allocation block per 1K of a 400K disk — i.e. about
//!    400 allocation blocks regardless of the disk's size, which is what forces
//!    2048-byte blocks on an 800K volume.
//! 3. The directory is about twelve sectors per 400K, grown to absorb any
//!    sectors that would otherwise be stranded between the last allocation
//!    block and the reserved tail (this is what makes the 800K directory 26
//!    sectors rather than 24).
//!
//! Deliberate differences from Apple's blanks: a volume formatted here is truly
//! empty, so `drNmFls` is 0 and `drNxtFNum` is 1 (Apple's blanks carry a single
//! invisible `Desktop` file, so they start at 1 file and file number 2), and
//! `drAtrb` is 0 (Apple's blanks set bit 6, which no documented MFS attribute
//! claims and which real Apple system disks leave clear).

use crate::blockmap::{BlockMap, packed_len};
use crate::dc42::Dc42Image;
use crate::error::{MfsError, Result};
use crate::mdb::{MDB_LEN, MFS_SIGNATURE, Mdb};
use crate::timestamp::MacTimestamp;
use crate::volume::{Container, ImageFormat, MfsVolume};

/// Logical sector size.
const SECTOR: u32 = 512;
/// Smallest volume worth formatting.
const MIN_SIZE: u32 = 100 * 1024;
/// Block numbers 2..=0xFEF are addressable in a 12-bit map entry.
const MAX_BLOCKS: u32 = 0xFEF - 1;
/// Sectors left unused at the end of the disk, as Apple's formatter does.
const RESERVED_TAIL_SECTORS: u32 = 2;
/// Target number of allocation blocks; sets the allocation block size.
const TARGET_BLOCKS: u32 = 400;
/// Directory sectors per 400K of disk, before slack is absorbed.
const DIR_SECTORS_PER_400K: u32 = 12;
/// `drClpSiz`, in allocation blocks.
const CLUMP_BLOCKS: u32 = 8;

/// The computed layout of a new volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    dir_st: u16,
    dir_len: u16,
    al_bl_st: u16,
    n_blocks: u16,
    al_blk_siz: u32,
}

/// Number of packed bytes needed for `n` 12-bit map entries, in `u32` maths so
/// that an implausible size cannot overflow [`crate::blockmap::packed_len`]'s
/// `u16` argument before the block count has been range-checked.
fn map_bytes(n_blocks: u32) -> u32 {
    (n_blocks * 3).div_ceil(2)
}

fn geometry(size_bytes: u32) -> Result<Geometry> {
    if !size_bytes.is_multiple_of(SECTOR) {
        return Err(MfsError::InvalidGeometry(format!(
            "{size_bytes} bytes is not a multiple of the {SECTOR}-byte sector size"
        )));
    }
    if size_bytes < MIN_SIZE {
        return Err(MfsError::InvalidGeometry(format!(
            "{size_bytes} bytes is below the {MIN_SIZE}-byte minimum volume size"
        )));
    }

    let sectors = size_bytes / SECTOR;
    let usable = sectors - RESERVED_TAIL_SECTORS;
    // Rule 2: about TARGET_BLOCKS allocation blocks, block size a power-of-two
    // multiple of the sector size, never smaller than two sectors.
    let per_block = (sectors / TARGET_BLOCKS).next_power_of_two().max(2);
    let al_blk_siz = per_block * SECTOR;
    // Rule 3, first half: the directory's nominal size.
    let dir_len = (sectors * DIR_SECTORS_PER_400K / 800).max(1);

    // The map's size depends on the block count, which depends on where the
    // allocation area starts, which depends on the map's size. Iterate: the
    // block count only ever shrinks, so this converges (in practice after one
    // round).
    let mut n_blocks = usable / per_block;
    let mut dir_st;
    let mut al_bl_st;
    loop {
        let mdb_sectors = (MDB_LEN as u32 + map_bytes(n_blocks)).div_ceil(SECTOR);
        dir_st = 2 + mdb_sectors;
        al_bl_st = dir_st + dir_len;
        if al_bl_st + per_block > usable {
            return Err(MfsError::InvalidGeometry(format!(
                "{size_bytes} bytes leaves no room for allocation blocks after the \
                 boot blocks, the {mdb_sectors}-sector MDB area and a \
                 {dir_len}-sector directory"
            )));
        }
        let next = (usable - al_bl_st) / per_block;
        if next >= n_blocks {
            // `next` can only exceed `n_blocks` on the first pass, where
            // `n_blocks` was a deliberate over-estimate.
            n_blocks = n_blocks.min(next);
            break;
        }
        n_blocks = next;
    }

    // Rule 3, second half: hand the directory whatever sectors the allocation
    // area could not use.
    let slack = usable - (al_bl_st + n_blocks * per_block);
    let dir_len = dir_len + slack;
    let al_bl_st = al_bl_st + slack;

    if n_blocks == 0 || n_blocks > MAX_BLOCKS {
        return Err(MfsError::InvalidGeometry(format!(
            "{size_bytes} bytes needs {n_blocks} allocation blocks of {al_blk_siz} \
             bytes; MFS supports 1..={MAX_BLOCKS}"
        )));
    }
    debug_assert!(al_bl_st + n_blocks * per_block <= sectors);

    Ok(Geometry {
        dir_st: dir_st as u16,
        dir_len: dir_len as u16,
        al_bl_st: al_bl_st as u16,
        n_blocks: n_blocks as u16,
        al_blk_siz,
    })
}

/// Create an empty volume of `size_bytes` named `name`.
pub(crate) fn format(size_bytes: u32, name: &str, format: ImageFormat) -> Result<MfsVolume> {
    let raw_name = crate::volume::encode_volume_name(name)?;
    if format == ImageFormat::DiskCopy42
        && size_bytes != MfsVolume::FLOPPY_400K
        && size_bytes != MfsVolume::FLOPPY_800K
    {
        return Err(MfsError::InvalidGeometry(format!(
            "DiskCopy 4.2 only defines the standard floppy geometries: {size_bytes} \
             bytes is neither {} nor {}",
            MfsVolume::FLOPPY_400K,
            MfsVolume::FLOPPY_800K
        )));
    }
    let g = geometry(size_bytes)?;

    let mut name_raw = [0u8; 28];
    name_raw[0] = raw_name.len() as u8;
    name_raw[1..1 + raw_name.len()].copy_from_slice(&raw_name);

    let now = MacTimestamp::now().0;
    let mdb = Mdb {
        sig_word: MFS_SIGNATURE,
        cr_date: now,
        ls_mod: now,
        atrb: 0,
        nm_fls: 0,
        dir_st: g.dir_st,
        dir_len: g.dir_len,
        nm_al_blks: g.n_blocks,
        al_blk_siz: g.al_blk_siz,
        clp_siz: g.al_blk_siz * CLUMP_BLOCKS,
        al_bl_st: g.al_bl_st,
        nxt_fnum: 1,
        free_bks: g.n_blocks,
        name_raw,
    };

    // The boot blocks stay zeroed (this is a data disk, not a bootable one),
    // and so does the file directory, whose zero flags bytes read as "no
    // entries here". The MDB and the all-free allocation block map are written
    // explicitly rather than left implicit in the zero fill.
    let mut data = vec![0u8; size_bytes as usize];
    mdb.write_to(&mut data[2 * SECTOR as usize..]);
    let map_start = 2 * SECTOR as usize + MDB_LEN;
    let map = BlockMap::new_empty(g.n_blocks);
    map.pack(&mut data[map_start..map_start + packed_len(g.n_blocks)]);

    match format {
        ImageFormat::Raw => MfsVolume::from_parts(data, Container::Raw),
        ImageFormat::DiskCopy42 => {
            let img = Dc42Image::new_blank(&raw_name, data)?;
            MfsVolume::from_parts(
                img.data,
                Container::Dc42 {
                    name: img.name,
                    tags: img.tags,
                    disk_format: img.disk_format,
                    format_byte: img.format_byte,
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_400k_geometry() {
        let g = geometry(MfsVolume::FLOPPY_400K).unwrap();
        assert_eq!(
            g,
            Geometry { dir_st: 4, dir_len: 12, al_bl_st: 16, n_blocks: 391, al_blk_siz: 1024 }
        );
    }

    #[test]
    fn apple_800k_geometry() {
        let g = geometry(MfsVolume::FLOPPY_800K).unwrap();
        assert_eq!(
            g,
            Geometry { dir_st: 4, dir_len: 26, al_bl_st: 30, n_blocks: 392, al_blk_siz: 2048 }
        );
    }

    #[test]
    fn geometry_is_self_consistent_across_sizes() {
        for sectors in [200u32, 201, 400, 800, 801, 1600, 2880, 4000, 20_000] {
            let size = sectors * SECTOR;
            if size < MIN_SIZE {
                continue;
            }
            let g = geometry(size).unwrap();
            // The MDB area holds the MDB plus the whole map.
            let mdb_sectors = g.dir_st as u32 - 2;
            assert!(
                MDB_LEN as u32 + map_bytes(g.n_blocks as u32) <= mdb_sectors * SECTOR,
                "map does not fit for {size} bytes"
            );
            // Regions abut, and everything is inside the disk.
            assert_eq!(g.al_bl_st as u32, g.dir_st as u32 + g.dir_len as u32);
            let end = g.al_bl_st as u32 + g.n_blocks as u32 * (g.al_blk_siz / SECTOR);
            assert!(end <= sectors, "allocation area overruns the disk for {size} bytes");
            // At most the reserved tail is wasted.
            assert_eq!(sectors - end, RESERVED_TAIL_SECTORS, "slack not absorbed for {size}");
            assert!(g.n_blocks as u32 <= MAX_BLOCKS);
        }
    }

    #[test]
    fn rejects_bad_sizes() {
        assert!(matches!(geometry(409_601), Err(MfsError::InvalidGeometry(_))));
        assert!(matches!(geometry(0), Err(MfsError::InvalidGeometry(_))));
        assert!(matches!(geometry(50 * 1024), Err(MfsError::InvalidGeometry(_))));
    }

    #[test]
    fn dc42_rejects_non_floppy_sizes() {
        let err = format(200 * 1024, "Odd", ImageFormat::DiskCopy42).unwrap_err();
        assert!(matches!(err, MfsError::InvalidGeometry(_)), "{err}");
        // ...but raw is happy with it.
        assert!(format(200 * 1024, "Odd", ImageFormat::Raw).is_ok());
    }

    #[test]
    fn formatted_volume_is_empty_and_consistent() {
        let vol = format(MfsVolume::FLOPPY_400K, "Untitled", ImageFormat::Raw).unwrap();
        let info = vol.info();
        assert_eq!(info.name, "Untitled");
        assert_eq!(info.file_count, 0);
        assert_eq!(info.total_blocks, 391);
        assert_eq!(info.free_blocks, 391);
        assert_eq!(info.alloc_block_size, 1024);
        assert_eq!(vol.files().count(), 0);
        assert!(vol.check().is_empty(), "{:?}", vol.check());
    }

    #[test]
    fn rejects_bad_volume_names() {
        for bad in ["", "Has:Colon", "twenty-eight characters long"] {
            assert!(
                matches!(
                    format(MfsVolume::FLOPPY_400K, bad, ImageFormat::Raw),
                    Err(MfsError::InvalidName(_))
                ),
                "{bad:?} should be rejected"
            );
        }
        // Exactly 27 bytes is the limit.
        assert!(format(MfsVolume::FLOPPY_400K, "twenty-seven characters ok!", ImageFormat::Raw).is_ok());
    }
}
