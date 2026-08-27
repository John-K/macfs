//! [`MfsVolume`] — the public API.
//!
//! A volume is held entirely in memory: [`open`](MfsVolume::open) reads the
//! whole image (unwrapping a DiskCopy 4.2 container if there is one), every
//! query and mutation works on the parsed structures plus the raw sector
//! buffer, and [`save_to`](MfsVolume::save_to) re-serializes the Master
//! Directory Block, the allocation block map and the file directory back into
//! that buffer before writing it out.
//!
//! **Preservation invariant:** opening an image and saving it again without
//! mutating anything reproduces the original bytes exactly — boot blocks,
//! unparsed MDB fields, the padding beyond the allocation block map, the
//! per-entry fields this crate never interprets, and the DiskCopy 4.2 header
//! name, tags and format bytes all survive verbatim.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::blockmap::{self, BlockMap};
use crate::dc42::Dc42Image;
use crate::dir::{self, RawEntry, SECTOR};
use crate::error::{MfsError, Result};
use crate::macroman;
use crate::mdb::{HFS_SIGNATURE, MDB_LEN, MFS_SIGNATURE, Mdb};
use crate::mkfs;
use crate::timestamp::MacTimestamp;
use crate::util::rd_u16;

/// Bytes of boot code at the very start of a volume: logical sectors 0 and 1.
const BOOT_BLOCKS_LEN: usize = 2 * SECTOR;
/// Byte offset of the Master Directory Block within the volume.
const MDB_OFFSET: usize = 2 * SECTOR;
/// Byte offset of the allocation block map, immediately after the MDB.
const MAP_OFFSET: usize = MDB_OFFSET + MDB_LEN;

/// `drAtrb` bit 15 — the volume is locked by hardware.
const ATRB_HW_LOCKED: u16 = 0x8000;
/// `drAtrb` bit 7 — the volume is locked by software.
const ATRB_SW_LOCKED: u16 = 0x0080;

/// `flFlags` bit 7 — the directory entry is in use.
const FLAG_IN_USE: u8 = 0x80;
/// `flFlags` bit 0 — the file is locked.
const FLAG_LOCKED: u8 = 0x01;

/// Longest file name MFS can store, in MacRoman bytes.
const MAX_FILE_NAME: usize = 63;
/// Longest volume name MFS can store, in MacRoman bytes.
const MAX_VOLUME_NAME: usize = 27;

/// The container an image is stored in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    /// A bare sector image.
    Raw,
    /// An image wrapped in an 84-byte DiskCopy 4.2 header, optionally with tags.
    DiskCopy42,
}

/// Which of a file's two forks to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fork {
    /// The data fork.
    Data,
    /// The resource fork.
    Resource,
}

/// A read-only view of one file in the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// File name, decoded from MacRoman.
    pub name: String,
    /// `flFNum` — the volume-unique file number.
    pub file_num: u32,
    /// `flTyp` — the four-character Finder type, e.g. `TEXT`.
    pub type_code: [u8; 4],
    /// `flCr` — the four-character Finder creator, e.g. `MACA`.
    pub creator: [u8; 4],
    /// `flFndrFlags` — Finder flags (invisible, bundle, locked, …).
    pub finder_flags: u16,
    /// Whether `flFlags` bit 0 is set, which blocks writes and deletion.
    pub locked: bool,
    /// `flCrDat` — creation date.
    pub created: MacTimestamp,
    /// `flMdDat` — last modification date.
    pub modified: MacTimestamp,
    /// `flDFLogLen` — bytes of real data in the data fork.
    pub data_len: u32,
    /// `flDFAllocLen` — bytes allocated to the data fork.
    pub data_alloc: u32,
    /// `flRFLogLen` — bytes of real data in the resource fork.
    pub rsrc_len: u32,
    /// `flRFAllocLen` — bytes allocated to the resource fork.
    pub rsrc_alloc: u32,
}

impl FileEntry {
    fn from_raw(e: &RawEntry) -> FileEntry {
        FileEntry {
            name: macroman::decode(&e.name),
            file_num: e.fnum,
            type_code: e.type_code,
            creator: e.creator,
            finder_flags: e.fndr_flags,
            locked: e.locked(),
            created: MacTimestamp(e.cr_dat),
            modified: MacTimestamp(e.md_dat),
            data_len: e.df_log_len,
            data_alloc: e.df_alloc_len,
            rsrc_len: e.rf_log_len,
            rsrc_alloc: e.rf_alloc_len,
        }
    }
}

/// Volume-level metadata, as reported by [`MfsVolume::info`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `drVN` — the volume name, decoded from MacRoman.
    pub name: String,
    /// `drCrDate` — when the volume was formatted.
    pub created: MacTimestamp,
    /// `drLsMod` — when the volume was last modified.
    pub modified: MacTimestamp,
    /// `drNmFls` — the number of files in the directory.
    pub file_count: u16,
    /// `drAlBlkSiz` — the allocation block size in bytes.
    pub alloc_block_size: u32,
    /// `drNmAlBlks` — the total number of allocation blocks.
    pub total_blocks: u16,
    /// `drFreeBks` — the number of unused allocation blocks.
    pub free_blocks: u16,
    /// The container the image was read from, and will be written back as.
    pub format: ImageFormat,
}

/// Everything needed to rebuild the container an image came in, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Container {
    Raw,
    Dc42 {
        name: Vec<u8>,
        tags: Vec<u8>,
        disk_format: u8,
        format_byte: u8,
    },
}

impl Container {
    fn format(&self) -> ImageFormat {
        match self {
            Container::Raw => ImageFormat::Raw,
            Container::Dc42 { .. } => ImageFormat::DiskCopy42,
        }
    }
}

/// An MFS volume, held in memory.
///
/// See the [crate documentation](crate) for a worked example.
#[derive(Debug, Clone)]
pub struct MfsVolume {
    /// The raw sector image, with any container header stripped.
    data: Vec<u8>,
    container: Container,
    mdb: Mdb,
    map: BlockMap,
    /// Directory entries, in the order they appear on disk.
    entries: Vec<RawEntry>,
    /// Whether any entry has been added, removed or altered since the image was
    /// read. The directory region is only repacked when it has, because real
    /// disks do not always pack their entries the way a fresh first-fit pass
    /// would — an entry that would fit in an earlier sector's tail can sit at
    /// the head of the next one — and rebuilding such a directory unchanged
    /// would move entries and rewrite bytes nobody asked to change.
    dir_dirty: bool,
}

impl MfsVolume {
    /// Bytes in a single-sided 400K floppy image.
    pub const FLOPPY_400K: u32 = 409_600;
    /// Bytes in a double-sided 800K floppy image.
    pub const FLOPPY_800K: u32 = 819_200;

    // ---------------------------------------------------------------- opening

    /// Read a volume, autodetecting a raw image versus a DiskCopy 4.2 container.
    ///
    /// The reader is rewound to its start and consumed to the end.
    pub fn open<R: Read + Seek>(r: R) -> Result<Self> {
        Self::from_image_bytes(read_all(r)?, None)
    }

    /// Read a volume, forcing the interpretation of the container.
    pub fn open_with_format<R: Read + Seek>(r: R, format: ImageFormat) -> Result<Self> {
        Self::from_image_bytes(read_all(r)?, Some(format))
    }

    /// Read a volume from a file, autodetecting the container.
    pub fn open_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_image_bytes(std::fs::read(path)?, None)
    }

    /// Create a brand new, empty volume of `size_bytes`.
    ///
    /// `size_bytes` must be a multiple of 512 and at least 100K. The DiskCopy
    /// 4.2 container only defines the standard floppy geometries, so
    /// [`ImageFormat::DiskCopy42`] additionally requires exactly
    /// [`FLOPPY_400K`](Self::FLOPPY_400K) or [`FLOPPY_800K`](Self::FLOPPY_800K)
    /// bytes.
    pub fn format(size_bytes: u32, name: &str, format: ImageFormat) -> Result<Self> {
        mkfs::format(size_bytes, name, format)
    }

    fn from_image_bytes(bytes: Vec<u8>, want: Option<ImageFormat>) -> Result<Self> {
        let format = match want {
            Some(f) => f,
            None if Dc42Image::detect(&bytes) => ImageFormat::DiskCopy42,
            None if looks_like_raw(&bytes) => ImageFormat::Raw,
            None if looks_like_raw_hfs(&bytes) => return Err(MfsError::UnsupportedHfs),
            None => return Err(MfsError::UnknownImageFormat),
        };
        match format {
            ImageFormat::Raw => Self::from_parts(bytes, Container::Raw),
            ImageFormat::DiskCopy42 => {
                let img = Dc42Image::parse(&bytes)?;
                Self::from_parts(
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

    /// Parse the MFS structures out of a bare sector image.
    pub(crate) fn from_parts(data: Vec<u8>, container: Container) -> Result<Self> {
        if data.len() < MDB_OFFSET + MDB_LEN {
            return Err(MfsError::CorruptVolume(format!(
                "image is {} bytes, too short to hold an MDB at offset {MDB_OFFSET}",
                data.len()
            )));
        }
        let mdb = Mdb::parse(&data[MDB_OFFSET..])?;

        // Mdb::parse has already checked 3 <= dir_st and dir_st + dir_len <= al_bl_st.
        let dir_start = mdb.dir_st as usize * SECTOR;
        let dir_end = dir_start + mdb.dir_len as usize * SECTOR;
        if dir_end > data.len() {
            return Err(MfsError::CorruptVolume(format!(
                "file directory runs to byte {dir_end}, past the {}-byte image",
                data.len()
            )));
        }
        let alloc_end =
            mdb.al_bl_st as usize * SECTOR + mdb.nm_al_blks as usize * mdb.al_blk_siz as usize;
        if alloc_end > data.len() {
            return Err(MfsError::CorruptVolume(format!(
                "{} allocation blocks of {} bytes from sector {} run to byte {alloc_end}, \
                 past the {}-byte image",
                mdb.nm_al_blks,
                mdb.al_blk_siz,
                mdb.al_bl_st,
                data.len()
            )));
        }

        let map = BlockMap::unpack(&data[MAP_OFFSET..dir_start], mdb.nm_al_blks)?;
        let entries = dir::parse_directory(&data[dir_start..dir_end])?;

        Ok(MfsVolume { data, container, mdb, map, entries, dir_dirty: false })
    }

    // ---------------------------------------------------------------- saving

    /// Serialize the volume and write it to `w` in its original container
    /// format.
    ///
    /// `w` is rewound to its start first; it is not truncated, so writing a
    /// small volume over a larger existing file leaves the tail of that file
    /// behind (use [`save_path`](Self::save_path), which truncates).
    pub fn save_to<W: Write + Seek>(&mut self, mut w: W) -> Result<()> {
        self.serialize()?;
        w.seek(SeekFrom::Start(0))?;
        match &self.container {
            Container::Raw => w.write_all(&self.data)?,
            Container::Dc42 { name, tags, disk_format, format_byte } => {
                let img = Dc42Image {
                    name: name.clone(),
                    data: self.data.clone(),
                    tags: tags.clone(),
                    disk_format: *disk_format,
                    format_byte: *format_byte,
                };
                w.write_all(&img.to_bytes())?;
            }
        }
        w.flush()?;
        Ok(())
    }

    /// Serialize the volume and write it to a file, in its original container
    /// format. An existing file is truncated.
    pub fn save_path<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let file = std::fs::File::create(path)?;
        self.save_to(file)
    }

    /// Write the MDB, the allocation block map and the file directory back into
    /// the sector buffer.
    ///
    /// `drLsMod` is deliberately *not* touched here: the mutators maintain it,
    /// so saving an untouched volume reproduces its bytes exactly.
    fn serialize(&mut self) -> Result<()> {
        let dir_start = self.mdb.dir_st as usize * SECTOR;
        let dir_end = dir_start + self.mdb.dir_len as usize * SECTOR;
        self.mdb.write_to(&mut self.data[MDB_OFFSET..]);
        // Only the bytes the map actually owns are rewritten. The padding
        // between the map and the directory is left alone: some real disks keep
        // data there, and `pack` read-modify-writes the one nibble it does not
        // own in the final byte of an odd-length map.
        let map_end = MAP_OFFSET + blockmap::packed_len(self.map.len());
        self.map.pack(&mut self.data[MAP_OFFSET..map_end]);
        if self.dir_dirty {
            dir::write_directory(&self.entries, &mut self.data[dir_start..dir_end])?;
        }
        Ok(())
    }

    // --------------------------------------------------------------- querying

    /// Volume-level metadata.
    pub fn info(&self) -> VolumeInfo {
        VolumeInfo {
            name: self.name(),
            created: MacTimestamp(self.mdb.cr_date),
            modified: MacTimestamp(self.mdb.ls_mod),
            file_count: self.mdb.nm_fls,
            alloc_block_size: self.mdb.al_blk_siz,
            total_blocks: self.mdb.nm_al_blks,
            free_blocks: self.mdb.free_bks,
            format: self.container.format(),
        }
    }

    /// The volume's boot blocks: sectors 0 and 1, the 1024 bytes the ROM loads
    /// and jumps into when starting up from this disk.
    ///
    /// A bootable disk starts these with the signature `0x4C4B` (`'LK'`),
    /// followed by the boot block header and the boot code itself. Volumes made
    /// by [`format`](Self::format) leave the whole region zeroed — a plain data
    /// disk. To make an image bootable, copy the region out of a real system
    /// disk with this method and install it with
    /// [`set_boot_blocks`](Self::set_boot_blocks).
    pub fn boot_blocks(&self) -> &[u8] {
        &self.data[..BOOT_BLOCKS_LEN]
    }

    /// Every file in the directory, in on-disk order.
    pub fn files(&self) -> impl Iterator<Item = FileEntry> + '_ {
        self.entries.iter().map(FileEntry::from_raw)
    }

    /// Look up one file by name, case-insensitively.
    pub fn file(&self, name: &str) -> Result<FileEntry> {
        Ok(FileEntry::from_raw(&self.entries[self.index_of(name)?]))
    }

    /// Read one fork of one file.
    ///
    /// An empty fork yields an empty vector. A fork whose block chain is
    /// shorter than its logical length is reported as `CorruptVolume` rather
    /// than silently returning truncated data.
    pub fn read_fork(&self, name: &str, fork: Fork) -> Result<Vec<u8>> {
        let e = &self.entries[self.index_of(name)?];
        let (start, log_len) = match fork {
            Fork::Data => (e.df_st_blk, e.df_log_len),
            Fork::Resource => (e.rf_st_blk, e.rf_log_len),
        };
        if start == 0 {
            return Ok(Vec::new());
        }
        let blk_siz = self.mdb.al_blk_siz as usize;
        let chain = self.map.chain(start)?;
        let chain_bytes = chain.len() as u64 * blk_siz as u64;
        if chain_bytes < log_len as u64 {
            return Err(MfsError::CorruptVolume(format!(
                "{name}: {} fork holds {log_len} bytes but its block chain only \
                 covers {chain_bytes}",
                match fork {
                    Fork::Data => "data",
                    Fork::Resource => "resource",
                }
            )));
        }
        let mut out = Vec::with_capacity(log_len as usize);
        for b in chain {
            let off = self.block_offset(b);
            out.extend_from_slice(&self.data[off..off + blk_siz]);
        }
        out.truncate(log_len as usize);
        Ok(out)
    }

    /// Check the volume for internal inconsistencies.
    ///
    /// Returns one human-readable line per problem found; an empty vector means
    /// everything checked out. Nothing here is fatal — the intent is to report
    /// on a suspect image, not to refuse to work with it.
    pub fn check(&self) -> Vec<String> {
        let mut report = Vec::new();

        if self.mdb.nm_fls as usize != self.entries.len() {
            report.push(format!(
                "drNmFls says {} files, the directory holds {}",
                self.mdb.nm_fls,
                self.entries.len()
            ));
        }
        let free = self.map.free_count();
        if self.mdb.free_bks as u32 != free {
            report.push(format!(
                "drFreeBks says {} free blocks, the allocation map has {free}",
                self.mdb.free_bks
            ));
        }

        let blk_siz = self.mdb.al_blk_siz as u64;
        let mut owner: Vec<Option<String>> = vec![None; self.map.len() as usize];

        for e in &self.entries {
            let name = macroman::decode(&e.name);
            if !e.in_use() {
                report.push(format!(
                    "{name}: directory entry is not marked in use and would hide \
                     every entry after it"
                ));
            }
            for fork in [Fork::Data, Fork::Resource] {
                let (label, start, log_len, alloc_len) = match fork {
                    Fork::Data => ("data", e.df_st_blk, e.df_log_len, e.df_alloc_len),
                    Fork::Resource => ("resource", e.rf_st_blk, e.rf_log_len, e.rf_alloc_len),
                };
                if start == 0 {
                    if log_len != 0 || alloc_len != 0 {
                        report.push(format!(
                            "{name}: {label} fork has no blocks but claims \
                             {log_len} bytes ({alloc_len} allocated)"
                        ));
                    }
                    continue;
                }
                if log_len > alloc_len {
                    report.push(format!(
                        "{name}: {label} fork logical length {log_len} exceeds \
                         its allocated length {alloc_len}"
                    ));
                }
                let chain = match self.map.chain(start) {
                    Ok(c) => c,
                    Err(err) => {
                        report.push(format!("{name}: {label} fork chain is broken: {err}"));
                        continue;
                    }
                };
                let chain_bytes = chain.len() as u64 * blk_siz;
                if chain_bytes != alloc_len as u64 {
                    report.push(format!(
                        "{name}: {label} fork claims {alloc_len} allocated bytes but its \
                         chain of {} blocks holds {chain_bytes}",
                        chain.len()
                    ));
                }
                if (log_len as u64) > chain_bytes {
                    report.push(format!(
                        "{name}: {label} fork logical length {log_len} exceeds the \
                         {chain_bytes} bytes in its block chain"
                    ));
                }
                for b in chain {
                    let slot = b as usize - blockmap::FIRST_BLOCK as usize;
                    match &owner[slot] {
                        Some(other) => report.push(format!(
                            "{name}: {label} fork shares allocation block {b} with {other}"
                        )),
                        None => owner[slot] = Some(format!("{name} ({label} fork)")),
                    }
                }
            }
        }

        let orphans = (0..self.map.len())
            .filter(|&i| {
                let v = self.map.get(i + blockmap::FIRST_BLOCK);
                v != blockmap::FREE && v != blockmap::RESERVED && owner[i as usize].is_none()
            })
            .count();
        if orphans > 0 {
            report.push(format!(
                "{orphans} allocation blocks are marked in use but belong to no file"
            ));
        }

        report
    }

    // -------------------------------------------------------------- mutating

    /// Add a new, empty file.
    pub fn create_file(&mut self, name: &str, type_code: [u8; 4], creator: [u8; 4]) -> Result<()> {
        self.check_writable()?;
        let raw_name = encode_file_name(name)?;
        if self.lookup(name).is_some() {
            return Err(MfsError::FileExists(name.to_string()));
        }
        let now = MacTimestamp::now().0;
        self.entries.push(RawEntry {
            flags: FLAG_IN_USE,
            version: 0,
            type_code,
            creator,
            fndr_flags: 0,
            pos: 0,
            fldr_num: 0,
            fnum: self.mdb.nxt_fnum,
            df_st_blk: 0,
            df_log_len: 0,
            df_alloc_len: 0,
            rf_st_blk: 0,
            rf_log_len: 0,
            rf_alloc_len: 0,
            cr_dat: now,
            md_dat: now,
            name: raw_name,
        });
        if !dir::fits(&self.entries, self.mdb.dir_len as usize) {
            self.entries.pop();
            return Err(MfsError::DirectoryFull);
        }
        self.mdb.nxt_fnum = self.mdb.nxt_fnum.wrapping_add(1);
        self.mdb.ls_mod = now;
        self.dir_dirty = true;
        self.recompute_counters();
        Ok(())
    }

    /// Replace the entire contents of one fork.
    ///
    /// The old blocks are released and a fresh chain is allocated. If the
    /// volume cannot hold `data` the volume is left completely untouched and
    /// `VolumeFull` is returned.
    pub fn write_fork(&mut self, name: &str, fork: Fork, data: &[u8]) -> Result<()> {
        self.check_writable()?;
        let idx = self.index_of(name)?;
        if self.entries[idx].locked() {
            return Err(MfsError::FileLocked(name.to_string()));
        }

        let blk_siz = self.mdb.al_blk_siz as usize;
        let needed = data.len().div_ceil(blk_siz) as u32;
        let old_start = match fork {
            Fork::Data => self.entries[idx].df_st_blk,
            Fork::Resource => self.entries[idx].rf_st_blk,
        };
        let old_len = if old_start == 0 {
            0
        } else {
            self.map.chain(old_start)?.len() as u32
        };
        let available = self.map.free_count() + old_len;
        if available < needed {
            return Err(MfsError::VolumeFull { needed_blocks: needed, free_blocks: available });
        }

        if old_start != 0 {
            self.map.free_chain(old_start)?;
        }
        let blocks = self.map.allocate(needed)?;
        for (i, &b) in blocks.iter().enumerate() {
            let off = self.block_offset(b);
            let taken = i * blk_siz;
            let n = data.len().saturating_sub(taken).min(blk_siz);
            let dst = &mut self.data[off..off + blk_siz];
            dst[..n].copy_from_slice(&data[taken..taken + n]);
            dst[n..].fill(0);
        }

        let start = blocks.first().copied().unwrap_or(0);
        let log_len = data.len() as u32;
        let alloc_len = needed * blk_siz as u32;
        let now = MacTimestamp::now().0;
        let e = &mut self.entries[idx];
        match fork {
            Fork::Data => {
                e.df_st_blk = start;
                e.df_log_len = log_len;
                e.df_alloc_len = alloc_len;
            }
            Fork::Resource => {
                e.rf_st_blk = start;
                e.rf_log_len = log_len;
                e.rf_alloc_len = alloc_len;
            }
        }
        e.md_dat = now;
        self.mdb.ls_mod = now;
        self.dir_dirty = true;
        self.recompute_counters();
        Ok(())
    }

    /// Delete a file, releasing both of its forks.
    pub fn delete_file(&mut self, name: &str) -> Result<()> {
        self.check_writable()?;
        let idx = self.index_of(name)?;
        if self.entries[idx].locked() {
            return Err(MfsError::FileLocked(name.to_string()));
        }
        let (df, rf) = (self.entries[idx].df_st_blk, self.entries[idx].rf_st_blk);
        if df != 0 {
            self.map.free_chain(df)?;
        }
        if rf != 0 {
            self.map.free_chain(rf)?;
        }
        self.entries.remove(idx);
        self.mdb.ls_mod = MacTimestamp::now().0;
        self.dir_dirty = true;
        self.recompute_counters();
        Ok(())
    }

    /// Rename a file. The new name may differ from the old one only in case.
    pub fn rename_file(&mut self, old: &str, new: &str) -> Result<()> {
        self.check_writable()?;
        let raw_name = encode_file_name(new)?;
        let idx = self.index_of(old)?;
        if self.lookup(new).is_some_and(|other| other != idx) {
            return Err(MfsError::FileExists(new.to_string()));
        }
        let previous = std::mem::replace(&mut self.entries[idx].name, raw_name);
        if !dir::fits(&self.entries, self.mdb.dir_len as usize) {
            self.entries[idx].name = previous;
            return Err(MfsError::DirectoryFull);
        }
        self.mdb.ls_mod = MacTimestamp::now().0;
        self.dir_dirty = true;
        self.recompute_counters();
        Ok(())
    }

    /// Rename the volume itself.
    pub fn rename_volume(&mut self, name: &str) -> Result<()> {
        self.check_writable()?;
        let raw = encode_name(name, MAX_VOLUME_NAME)?;
        let mut name_raw = [0u8; 28];
        name_raw[0] = raw.len() as u8;
        name_raw[1..1 + raw.len()].copy_from_slice(&raw);
        self.mdb.name_raw = name_raw;
        self.mdb.ls_mod = MacTimestamp::now().0;
        self.recompute_counters();
        Ok(())
    }

    /// Replace the volume's boot blocks, typically with a copy taken from a
    /// real system disk via [`boot_blocks`](Self::boot_blocks).
    ///
    /// The bytes are stored verbatim; nothing here validates the boot code or
    /// its `0x4C4B` signature, and no filesystem structure depends on it.
    /// Because boot code is not filesystem metadata, `drLsMod` is deliberately
    /// left alone — Apple's own tools do not bump the volume's modification
    /// date when installing boot blocks either.
    pub fn set_boot_blocks(&mut self, blocks: &[u8; BOOT_BLOCKS_LEN]) -> Result<()> {
        self.check_writable()?;
        self.data[..BOOT_BLOCKS_LEN].copy_from_slice(blocks);
        Ok(())
    }

    /// Set a file's Finder type and creator codes.
    pub fn set_type_creator(
        &mut self,
        name: &str,
        type_code: [u8; 4],
        creator: [u8; 4],
    ) -> Result<()> {
        self.check_writable()?;
        let idx = self.index_of(name)?;
        self.entries[idx].type_code = type_code;
        self.entries[idx].creator = creator;
        self.mdb.ls_mod = MacTimestamp::now().0;
        self.dir_dirty = true;
        self.recompute_counters();
        Ok(())
    }

    /// Set or clear a file's locked bit.
    pub fn set_locked(&mut self, name: &str, locked: bool) -> Result<()> {
        self.check_writable()?;
        let idx = self.index_of(name)?;
        if locked {
            self.entries[idx].flags |= FLAG_LOCKED;
        } else {
            self.entries[idx].flags &= !FLAG_LOCKED;
        }
        self.mdb.ls_mod = MacTimestamp::now().0;
        self.dir_dirty = true;
        self.recompute_counters();
        Ok(())
    }

    /// Overwrite a file's creation and/or modification dates. `None` leaves a
    /// date as it is.
    pub fn set_times(
        &mut self,
        name: &str,
        created: Option<MacTimestamp>,
        modified: Option<MacTimestamp>,
    ) -> Result<()> {
        self.check_writable()?;
        let idx = self.index_of(name)?;
        if let Some(t) = created {
            self.entries[idx].cr_dat = t.0;
        }
        if let Some(t) = modified {
            self.entries[idx].md_dat = t.0;
        }
        self.mdb.ls_mod = MacTimestamp::now().0;
        self.dir_dirty = true;
        self.recompute_counters();
        Ok(())
    }

    // --------------------------------------------------------------- internals

    /// The single place `drFreeBks` and `drNmFls` are derived, so that no
    /// mutator can leave them stale.
    fn recompute_counters(&mut self) {
        self.mdb.free_bks = self.map.free_count() as u16;
        self.mdb.nm_fls = self.entries.len() as u16;
    }

    fn check_writable(&self) -> Result<()> {
        if self.mdb.atrb & (ATRB_HW_LOCKED | ATRB_SW_LOCKED) != 0 {
            return Err(MfsError::VolumeLocked);
        }
        Ok(())
    }

    fn name(&self) -> String {
        // Mdb::parse rejects a length byte above 27.
        let len = self.mdb.name_raw[0] as usize;
        macroman::decode(&self.mdb.name_raw[1..1 + len])
    }

    /// Index of the entry whose name matches `name`, case-insensitively.
    fn lookup(&self, name: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| macroman::eq_ignore_case(&macroman::decode(&e.name), name))
    }

    fn index_of(&self, name: &str) -> Result<usize> {
        self.lookup(name)
            .ok_or_else(|| MfsError::FileNotFound(name.to_string()))
    }

    /// Byte offset of allocation block `block` within the sector image.
    fn block_offset(&self, block: u16) -> usize {
        let per_block = self.mdb.al_blk_siz as usize / SECTOR;
        (self.mdb.al_bl_st as usize
            + (block as usize - blockmap::FIRST_BLOCK as usize) * per_block)
            * SECTOR
    }
}

/// The would-be `drSigWord` of a bare sector image, if `bytes` is big enough
/// to hold an MDB at all.
fn raw_signature(bytes: &[u8]) -> Option<u16> {
    (bytes.len() >= MDB_OFFSET + MDB_LEN).then(|| rd_u16(bytes, MDB_OFFSET))
}

/// Whether `bytes` could be a bare MFS sector image.
fn looks_like_raw(bytes: &[u8]) -> bool {
    bytes.len().is_multiple_of(SECTOR) && raw_signature(bytes) == Some(MFS_SIGNATURE)
}

/// Whether `bytes` looks like a bare HFS sector image (reported as unsupported).
fn looks_like_raw_hfs(bytes: &[u8]) -> bool {
    raw_signature(bytes) == Some(HFS_SIGNATURE)
}

fn read_all<R: Read + Seek>(mut r: R) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    r.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Encode and validate a name destined for a Pascal string on disk.
fn encode_name(name: &str, max: usize) -> Result<Vec<u8>> {
    let bytes = macroman::encode(name).map_err(|c| {
        MfsError::InvalidName(format!("{name:?} contains {c:?}, which MacRoman cannot represent"))
    })?;
    if bytes.is_empty() {
        return Err(MfsError::InvalidName("name is empty".to_string()));
    }
    if bytes.len() > max {
        return Err(MfsError::InvalidName(format!(
            "{name:?} is {} bytes, the maximum is {max}",
            bytes.len()
        )));
    }
    if let Some(bad) = bytes.iter().find(|&&b| b == b':' || b == 0) {
        return Err(MfsError::InvalidName(format!(
            "{name:?} contains a forbidden byte {bad:#04x}"
        )));
    }
    Ok(bytes)
}

fn encode_file_name(name: &str) -> Result<Vec<u8>> {
    encode_name(name, MAX_FILE_NAME)
}

/// Validate and encode a volume name — used by [`crate::mkfs`].
pub(crate) fn encode_volume_name(name: &str) -> Result<Vec<u8>> {
    encode_name(name, MAX_VOLUME_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

    /// Real-world images fetched by `scripts/fetch-test-images.sh`. They are
    /// Apple-copyrighted and therefore never committed, so every test that
    /// wants one returns quietly when it is absent.
    const GOLDEN: [&str; 6] = [
        "Sample.img",
        "Finder 1.0.image",
        "1.1 System Disk.image",
        "2.0 System Disk.image",
        "gryphel-mfs400k.image",
        "gryphel-mfs800k.image",
    ];

    fn golden_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/images")
            .join(name)
    }

    fn golden(name: &str) -> Option<Vec<u8>> {
        std::fs::read(golden_path(name)).ok()
    }

    fn save_bytes(vol: &mut MfsVolume) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        vol.save_to(&mut out).expect("save");
        out.into_inner()
    }

    /// Deterministic filler so a misplaced block is impossible to miss.
    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    // ------------------------------------------------------------- golden images

    #[test]
    fn sample_image_lists_its_files() {
        let Some(bytes) = golden("Sample.img") else { return };
        // Despite the name it is a DiskCopy 4.2 container, with tags.
        let vol = MfsVolume::open(Cursor::new(bytes)).expect("open Sample.img");
        let info = vol.info();
        assert_eq!(info.format, ImageFormat::DiskCopy42);
        // "Write" is the DiskCopy header's image name; the volume itself is
        // called "Sample".
        assert_eq!(info.name, "Sample");
        assert_eq!(info.alloc_block_size, 1024);
        assert_eq!(info.total_blocks, 391);
        assert_eq!(info.file_count as usize, vol.files().count());
        assert!(info.file_count > 0);
        for f in vol.files() {
            assert!(!f.name.is_empty());
            // Every listed file is findable by its own name, in any case.
            assert_eq!(vol.file(&f.name).unwrap(), f);
            assert_eq!(vol.file(&f.name.to_uppercase()).unwrap(), f);
        }
        assert!(vol.check().is_empty(), "{:?}", vol.check());
    }

    /// The keystone preservation test: open → save must reproduce the input
    /// image byte for byte, container header and all.
    #[test]
    fn golden_images_survive_open_save_byte_identically() {
        let mut checked = 0;
        for name in GOLDEN {
            let Some(original) = golden(name) else { continue };
            let mut vol = MfsVolume::open_path(golden_path(name)).unwrap_or_else(|e| {
                panic!("{name}: {e}");
            });
            let saved = save_bytes(&mut vol);
            assert_eq!(saved.len(), original.len(), "{name}: length changed");
            if saved != original {
                let at = saved
                    .iter()
                    .zip(&original)
                    .position(|(a, b)| a != b)
                    .unwrap();
                panic!(
                    "{name}: first difference at byte {at}: saved {:#04x}, original {:#04x}",
                    saved[at], original[at]
                );
            }
            // Saving again is idempotent too.
            assert_eq!(save_bytes(&mut vol), original, "{name}: second save differs");
            checked += 1;
        }
        eprintln!("open/save byte-identical over {checked} golden image(s)");
    }

    #[test]
    fn golden_images_read_every_fork() {
        for name in GOLDEN {
            let Some(bytes) = golden(name) else { continue };
            let vol = MfsVolume::open(Cursor::new(bytes)).unwrap_or_else(|e| panic!("{name}: {e}"));
            let problems = vol.check();
            assert!(problems.is_empty(), "{name}: {problems:?}");
            for f in vol.files() {
                let data = vol
                    .read_fork(&f.name, Fork::Data)
                    .unwrap_or_else(|e| panic!("{name}: {} data fork: {e}", f.name));
                let rsrc = vol
                    .read_fork(&f.name, Fork::Resource)
                    .unwrap_or_else(|e| panic!("{name}: {} resource fork: {e}", f.name));
                assert_eq!(data.len() as u32, f.data_len, "{name}: {}", f.name);
                assert_eq!(rsrc.len() as u32, f.rsrc_len, "{name}: {}", f.name);
            }
        }
    }

    /// Our formatter must agree with Apple's, field for field, on the geometry.
    #[test]
    fn mkfs_matches_apples_blank_geometry() {
        for (name, size) in [
            ("gryphel-mfs400k.image", MfsVolume::FLOPPY_400K),
            ("gryphel-mfs800k.image", MfsVolume::FLOPPY_800K),
        ] {
            let Some(bytes) = golden(name) else { continue };
            let apple = MfsVolume::open(Cursor::new(bytes)).unwrap().mdb;
            let ours = MfsVolume::format(size, "Untitled", ImageFormat::Raw).unwrap().mdb;

            assert_eq!(ours.sig_word, apple.sig_word, "{name}");
            assert_eq!(ours.dir_st, apple.dir_st, "{name}: drDirSt");
            assert_eq!(ours.dir_len, apple.dir_len, "{name}: drDirLen");
            assert_eq!(ours.nm_al_blks, apple.nm_al_blks, "{name}: drNmAlBlks");
            assert_eq!(ours.al_blk_siz, apple.al_blk_siz, "{name}: drAlBlkSiz");
            assert_eq!(ours.clp_siz, apple.clp_siz, "{name}: drClpSiz");
            assert_eq!(ours.al_bl_st, apple.al_bl_st, "{name}: drAlBlSt");
            assert_eq!(ours.name_raw, apple.name_raw, "{name}: drVN");

            // Documented, deliberate differences: Apple's blanks are not empty.
            // They carry one invisible `Desktop` file occupying one allocation
            // block, so their counters start one higher.
            assert_eq!(apple.nm_fls, 1);
            assert_eq!(ours.nm_fls, 0);
            assert_eq!(apple.nxt_fnum, 2);
            assert_eq!(ours.nxt_fnum, 1);
            assert_eq!(apple.free_bks, apple.nm_al_blks - 1);
            assert_eq!(ours.free_bks, ours.nm_al_blks);
            // ...and they set drAtrb bit 6, which no documented attribute claims.
            assert_eq!(apple.atrb, 0x0040);
            assert_eq!(ours.atrb, 0);
        }
    }

    // -------------------------------------------------------------- round trips

    #[test]
    fn create_write_save_reopen_round_trip() {
        for (size, format) in [
            (MfsVolume::FLOPPY_400K, ImageFormat::Raw),
            (MfsVolume::FLOPPY_400K, ImageFormat::DiskCopy42),
            (MfsVolume::FLOPPY_800K, ImageFormat::Raw),
        ] {
            let mut vol = MfsVolume::format(size, "Scratch", format).unwrap();
            let blk = vol.info().alloc_block_size as usize;
            let cases: Vec<(String, Vec<u8>, Vec<u8>)> = [0usize, 1, blk - 1, blk, blk + 1, 100_000]
                .iter()
                .enumerate()
                .map(|(i, &n)| {
                    (
                        format!("File {i}"),
                        pattern(n, i as u32 + 0x1234),
                        pattern(n / 2, i as u32 + 0x9876),
                    )
                })
                .collect();

            for (name, data, rsrc) in &cases {
                vol.create_file(name, *b"TEXT", *b"MACA").unwrap();
                vol.write_fork(name, Fork::Data, data).unwrap();
                vol.write_fork(name, Fork::Resource, rsrc).unwrap();
            }
            assert!(vol.check().is_empty(), "{:?}", vol.check());

            let image = save_bytes(&mut vol);
            let back = MfsVolume::open(Cursor::new(image)).unwrap();
            assert_eq!(back.info().format, format);
            assert_eq!(back.info().name, "Scratch");
            assert_eq!(back.info().file_count as usize, cases.len());
            assert!(back.check().is_empty(), "{:?}", back.check());

            for (i, (name, data, rsrc)) in cases.iter().enumerate() {
                assert_eq!(&back.read_fork(name, Fork::Data).unwrap(), data, "{name}");
                assert_eq!(&back.read_fork(name, Fork::Resource).unwrap(), rsrc, "{name}");
                let e = back.file(name).unwrap();
                assert_eq!(e.data_len as usize, data.len());
                assert_eq!(e.rsrc_len as usize, rsrc.len());
                assert_eq!(e.data_alloc as usize, data.len().div_ceil(blk) * blk);
                assert_eq!(e.file_num, i as u32 + 1, "file numbers count up from 1");
                assert_eq!(&e.type_code, b"TEXT");
            }
        }
    }

    #[test]
    fn delete_restores_free_blocks() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "Scratch", ImageFormat::Raw).unwrap();
        let before = vol.info().free_blocks;
        vol.create_file("Doomed", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Doomed", Fork::Data, &pattern(30_000, 7)).unwrap();
        vol.write_fork("Doomed", Fork::Resource, &pattern(5_000, 9)).unwrap();
        assert_eq!(vol.info().free_blocks, before - 30 - 5);
        assert_eq!(vol.info().file_count, 1);

        vol.delete_file("DOOMED").unwrap();
        assert_eq!(vol.info().free_blocks, before);
        assert_eq!(vol.info().file_count, 0);
        assert!(matches!(vol.file("Doomed"), Err(MfsError::FileNotFound(_))));
        assert!(vol.check().is_empty(), "{:?}", vol.check());

        // The file number is not recycled.
        vol.create_file("Fresh", *b"TEXT", *b"MACA").unwrap();
        assert_eq!(vol.file("Fresh").unwrap().file_num, 2);
    }

    #[test]
    fn overwriting_a_fork_frees_the_old_chain() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "Scratch", ImageFormat::Raw).unwrap();
        let free = vol.info().free_blocks;
        vol.create_file("Notes", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Notes", Fork::Data, &pattern(50_000, 3)).unwrap();
        assert_eq!(vol.info().free_blocks, free - 49);
        vol.write_fork("Notes", Fork::Data, b"tiny").unwrap();
        assert_eq!(vol.info().free_blocks, free - 1);
        assert_eq!(vol.read_fork("Notes", Fork::Data).unwrap(), b"tiny");
        // An empty write releases the fork entirely.
        vol.write_fork("Notes", Fork::Data, &[]).unwrap();
        assert_eq!(vol.info().free_blocks, free);
        assert!(vol.read_fork("Notes", Fork::Data).unwrap().is_empty());
        assert_eq!(vol.file("Notes").unwrap().data_alloc, 0);
        assert!(vol.check().is_empty(), "{:?}", vol.check());
    }

    // ------------------------------------------------------------------ errors

    #[test]
    fn volume_full_leaves_the_volume_untouched() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "Scratch", ImageFormat::Raw).unwrap();
        vol.create_file("Big", *b"TEXT", *b"MACA").unwrap();
        let keep = pattern(200_000, 5);
        vol.write_fork("Big", Fork::Data, &keep).unwrap();
        let before_free = vol.info().free_blocks;
        let before_image = save_bytes(&mut vol);

        // One byte more than the whole volume holds.
        match vol.write_fork("Big", Fork::Data, &vec![0u8; 391 * 1024 + 1]) {
            Err(MfsError::VolumeFull { needed_blocks, free_blocks }) => {
                assert_eq!(needed_blocks, 392);
                // The blocks the fork already owns count towards the write.
                assert_eq!(free_blocks, 391);
            }
            other => panic!("expected VolumeFull, got {other:?}"),
        }
        assert_eq!(vol.info().free_blocks, before_free);
        assert_eq!(vol.read_fork("Big", Fork::Data).unwrap(), keep);
        assert_eq!(save_bytes(&mut vol), before_image, "failed write changed the image");
    }

    #[test]
    fn directory_full_is_reported_before_anything_changes() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "Scratch", ImageFormat::Raw).unwrap();
        // 12 sectors hold nine 52-byte entries each.
        for i in 0..108 {
            vol.create_file(&format!("f{i}"), *b"TEXT", *b"MACA").unwrap();
        }
        assert_eq!(vol.info().file_count, 108);
        let before = save_bytes(&mut vol);
        assert!(matches!(vol.create_file("one too many", *b"TEXT", *b"MACA"), Err(MfsError::DirectoryFull)));
        assert_eq!(vol.info().file_count, 108);
        assert_eq!(save_bytes(&mut vol), before);
        // A rename that would grow an entry past the end fails the same way.
        assert!(matches!(
            vol.rename_file("f0", &"n".repeat(63)),
            Err(MfsError::DirectoryFull)
        ));
        assert_eq!(vol.file("f0").unwrap().name, "f0");
    }

    #[test]
    fn duplicate_and_invalid_names_are_rejected() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "Scratch", ImageFormat::Raw).unwrap();
        vol.create_file("README", *b"TEXT", *b"MACA").unwrap();
        assert!(matches!(
            vol.create_file("readme", *b"TEXT", *b"MACA"),
            Err(MfsError::FileExists(_))
        ));
        assert_eq!(vol.info().file_count, 1);

        for bad in ["", "has:colon", &"x".repeat(64), "\u{4F60}"] {
            assert!(
                matches!(vol.create_file(bad, *b"TEXT", *b"MACA"), Err(MfsError::InvalidName(_))),
                "{bad:?} should be rejected"
            );
        }
        // 63 bytes is fine, and so are MacRoman accents.
        vol.create_file(&"x".repeat(63), *b"TEXT", *b"MACA").unwrap();
        vol.create_file("Caf\u{e9}", *b"TEXT", *b"MACA").unwrap();
        assert_eq!(vol.file("CAF\u{c9}").unwrap().name, "Caf\u{e9}");

        assert!(matches!(vol.read_fork("nope", Fork::Data), Err(MfsError::FileNotFound(_))));
    }

    #[test]
    fn renaming_and_setters() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "Scratch", ImageFormat::Raw).unwrap();
        vol.create_file("Old Name", *b"TEXT", *b"MACA").unwrap();
        vol.create_file("Other", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Old Name", Fork::Data, b"payload").unwrap();

        assert!(matches!(vol.rename_file("Old Name", "OTHER"), Err(MfsError::FileExists(_))));
        // Renaming to a different case of the same name is allowed.
        vol.rename_file("Old Name", "OLD NAME").unwrap();
        vol.rename_file("old name", "New Name").unwrap();
        assert_eq!(vol.file("new name").unwrap().name, "New Name");
        assert_eq!(vol.read_fork("New Name", Fork::Data).unwrap(), b"payload");

        vol.set_type_creator("New Name", *b"APPL", *b"MPS ").unwrap();
        vol.set_times("New Name", Some(MacTimestamp(1)), Some(MacTimestamp(2))).unwrap();
        let e = vol.file("New Name").unwrap();
        assert_eq!((&e.type_code, &e.creator), (b"APPL", b"MPS "));
        assert_eq!((e.created, e.modified), (MacTimestamp(1), MacTimestamp(2)));
        assert!(!e.locked);

        vol.rename_volume("Renamed").unwrap();
        assert_eq!(vol.info().name, "Renamed");
        assert!(matches!(vol.rename_volume(&"v".repeat(28)), Err(MfsError::InvalidName(_))));
        assert_eq!(vol.info().name, "Renamed");

        // Everything survives a save/reopen.
        let back = MfsVolume::open(Cursor::new(save_bytes(&mut vol))).unwrap();
        assert_eq!(back.info().name, "Renamed");
        assert_eq!(back.file("New Name").unwrap(), e);
    }

    #[test]
    fn locks_are_honoured() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "Scratch", ImageFormat::Raw).unwrap();
        vol.create_file("Precious", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Precious", Fork::Data, b"keep me").unwrap();
        vol.set_locked("Precious", true).unwrap();
        assert!(vol.file("Precious").unwrap().locked);

        assert!(matches!(vol.delete_file("Precious"), Err(MfsError::FileLocked(_))));
        assert!(matches!(
            vol.write_fork("Precious", Fork::Data, b"nope"),
            Err(MfsError::FileLocked(_))
        ));
        assert_eq!(vol.read_fork("Precious", Fork::Data).unwrap(), b"keep me");

        vol.set_locked("Precious", false).unwrap();
        vol.delete_file("Precious").unwrap();

        // A software-locked volume refuses every mutation.
        vol.mdb.atrb |= 0x0080;
        assert!(matches!(vol.create_file("x", *b"TEXT", *b"MACA"), Err(MfsError::VolumeLocked)));
        assert!(matches!(vol.rename_volume("x"), Err(MfsError::VolumeLocked)));
        vol.mdb.atrb = 0x8000;
        assert!(matches!(vol.create_file("x", *b"TEXT", *b"MACA"), Err(MfsError::VolumeLocked)));
        // ...but reading still works.
        assert_eq!(vol.files().count(), 0);
    }

    // ------------------------------------------------------------- containers

    #[test]
    fn format_autodetection() {
        let mut raw = MfsVolume::format(MfsVolume::FLOPPY_400K, "R", ImageFormat::Raw).unwrap();
        let raw_bytes = save_bytes(&mut raw);
        assert_eq!(raw_bytes.len(), MfsVolume::FLOPPY_400K as usize);
        assert_eq!(
            MfsVolume::open(Cursor::new(raw_bytes.clone())).unwrap().info().format,
            ImageFormat::Raw
        );

        let mut dc = MfsVolume::format(MfsVolume::FLOPPY_400K, "D", ImageFormat::DiskCopy42).unwrap();
        let dc_bytes = save_bytes(&mut dc);
        // 84-byte header, disk data, and 12 tag bytes per sector.
        assert_eq!(dc_bytes.len(), 84 + 409_600 + 9_600);
        let reopened = MfsVolume::open(Cursor::new(dc_bytes.clone())).unwrap();
        assert_eq!(reopened.info().format, ImageFormat::DiskCopy42);
        assert_eq!(reopened.info().name, "D");
        // Re-saving is byte-identical for a fresh volume too.
        let mut reopened = reopened;
        assert_eq!(save_bytes(&mut reopened), dc_bytes);

        // Forcing the wrong interpretation is an error, not a wrong answer.
        assert!(MfsVolume::open_with_format(Cursor::new(dc_bytes), ImageFormat::Raw).is_err());
        assert!(MfsVolume::open_with_format(Cursor::new(raw_bytes), ImageFormat::DiskCopy42).is_err());
    }

    #[test]
    fn unrecognized_images_are_rejected() {
        assert!(matches!(
            MfsVolume::open(Cursor::new(vec![0u8; 409_600])),
            Err(MfsError::UnknownImageFormat)
        ));
        assert!(matches!(
            MfsVolume::open(Cursor::new(Vec::new())),
            Err(MfsError::UnknownImageFormat)
        ));
    }

    #[test]
    fn hfs_images_are_reported_unsupported() {
        // A raw HFS volume: MFS's MDB offset holds HFS's signature.
        let mut hfs = vec![0u8; 409_600];
        hfs[1024] = 0x42;
        hfs[1025] = 0x44;
        assert!(matches!(
            MfsVolume::open(Cursor::new(hfs.clone())),
            Err(MfsError::UnsupportedHfs)
        ));
        // Forcing the raw format reaches the same verdict via the MDB parse.
        assert!(matches!(
            MfsVolume::open_with_format(Cursor::new(hfs), ImageFormat::Raw),
            Err(MfsError::UnsupportedHfs)
        ));

        // A DiskCopy 4.2 container wrapping an HFS volume.
        let mut data = vec![0u8; 409_600];
        data[1024] = 0x42;
        data[1025] = 0x44;
        let dc_bytes = crate::dc42::Dc42Image::new_blank(b"HFS Disk", data)
            .unwrap()
            .to_bytes();
        assert!(matches!(
            MfsVolume::open(Cursor::new(dc_bytes)),
            Err(MfsError::UnsupportedHfs)
        ));
    }

    #[test]
    fn truncated_image_is_corrupt_not_a_panic() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "R", ImageFormat::Raw).unwrap();
        let bytes = save_bytes(&mut vol);
        // 1056: too short for the MDB. 2048/4096: the directory is cut off.
        // 8192/100_000/408_064: the allocation area is cut off.
        for len in [1024 + 32, 2048, 4096, 8192, 100_000, 408_064] {
            let err = MfsVolume::open_with_format(Cursor::new(bytes[..len].to_vec()), ImageFormat::Raw);
            assert!(err.is_err(), "a {len}-byte image should not open");
        }
    }

    #[test]
    fn check_reports_a_broken_chain() {
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "R", ImageFormat::Raw).unwrap();
        vol.create_file("Broken", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Broken", Fork::Data, &pattern(4000, 11)).unwrap();
        assert!(vol.check().is_empty());

        // Point the entry at a block that is not allocated at all.
        vol.entries[0].df_st_blk = 300;
        let problems = vol.check();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("chain is broken"), "{problems:?}");
        assert!(problems[1].contains("belong to no file"), "{problems:?}");
        assert!(matches!(vol.read_fork("Broken", Fork::Data), Err(MfsError::CorruptVolume(_))));

        // Stale counters are reported too.
        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "R", ImageFormat::Raw).unwrap();
        vol.mdb.nm_fls = 3;
        vol.mdb.free_bks = 7;
        assert_eq!(vol.check().len(), 2);
    }

    /// Writing to a real disk must leave everything else exactly as it was,
    /// even though the directory is repacked from scratch.
    #[test]
    fn mutating_a_golden_image_preserves_every_other_file() {
        let Some(bytes) = golden("Sample.img") else { return };
        let mut vol = MfsVolume::open(Cursor::new(bytes)).unwrap();
        let before: Vec<(FileEntry, Vec<u8>, Vec<u8>)> = vol
            .files()
            .map(|f| {
                let d = vol.read_fork(&f.name, Fork::Data).unwrap();
                let r = vol.read_fork(&f.name, Fork::Resource).unwrap();
                (f, d, r)
            })
            .collect();
        let victim = before[0].0.name.clone();
        let free_before = vol.info().free_blocks;

        vol.rename_file(&victim, "Renamed By macfs").unwrap();
        vol.create_file("Newcomer", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Newcomer", Fork::Data, &pattern(3_000, 21)).unwrap();

        let back = MfsVolume::open(Cursor::new(save_bytes(&mut vol))).unwrap();
        assert!(back.check().is_empty(), "{:?}", back.check());
        assert_eq!(back.info().file_count as usize, before.len() + 1);
        assert_eq!(back.info().free_blocks, free_before - 3);
        assert_eq!(back.read_fork("Newcomer", Fork::Data).unwrap(), pattern(3_000, 21));

        for (i, (f, data, rsrc)) in before.iter().enumerate() {
            let name = if i == 0 { "Renamed By macfs" } else { &f.name };
            let now = back.file(name).unwrap();
            assert_eq!(now.file_num, f.file_num, "{name}");
            assert_eq!(now.created, f.created, "{name}");
            assert_eq!(&back.read_fork(name, Fork::Data).unwrap(), data, "{name}");
            assert_eq!(&back.read_fork(name, Fork::Resource).unwrap(), rsrc, "{name}");
        }
        // The renamed file is gone under its old name.
        assert!(matches!(back.file(&victim), Err(MfsError::FileNotFound(_))));
    }

    #[test]
    fn boot_blocks_round_trip() {
        let mut vol =
            MfsVolume::format(MfsVolume::FLOPPY_400K, "Bootable", ImageFormat::Raw).unwrap();
        // A freshly formatted volume is a plain data disk.
        assert_eq!(vol.boot_blocks().len(), 1024);
        assert!(vol.boot_blocks().iter().all(|&b| b == 0));

        let mut code = [0u8; 1024];
        code[..2].copy_from_slice(&0x4C4Bu16.to_be_bytes()); // 'LK'
        code[2..1024].copy_from_slice(&pattern(1022, 0x5EED));
        vol.set_boot_blocks(&code).unwrap();
        assert_eq!(vol.boot_blocks(), code);

        // The filesystem is untouched by the swap.
        vol.create_file("Payload", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Payload", Fork::Data, b"still fine").unwrap();

        let back = MfsVolume::open(Cursor::new(save_bytes(&mut vol))).unwrap();
        assert_eq!(back.boot_blocks(), code);
        assert_eq!(&back.boot_blocks()[..2], b"LK");
        assert_eq!(back.read_fork("Payload", Fork::Data).unwrap(), b"still fine");
        assert!(back.check().is_empty(), "{:?}", back.check());

        // Installing boot code is not a filesystem modification.
        let mut fresh =
            MfsVolume::format(MfsVolume::FLOPPY_400K, "Bootable", ImageFormat::Raw).unwrap();
        let ls_mod = fresh.info().modified;
        fresh.set_boot_blocks(&code).unwrap();
        assert_eq!(fresh.info().modified, ls_mod, "drLsMod must not move");

        // A locked volume refuses the write.
        fresh.mdb.atrb = 0x8000;
        assert!(matches!(fresh.set_boot_blocks(&[0u8; 1024]), Err(MfsError::VolumeLocked)));
        assert_eq!(fresh.boot_blocks(), code);
    }

    /// Real system disks carry real boot code, which must survive untouched.
    #[test]
    fn golden_boot_blocks_are_preserved() {
        let Some(bytes) = golden("1.1 System Disk.image") else { return };
        let mut vol = MfsVolume::open(Cursor::new(bytes)).unwrap();
        let boot = vol.boot_blocks().to_vec();
        assert_eq!(&boot[..2], b"LK", "a system disk should be bootable");
        // Copy it onto a blank volume, the way `mfs mkfs --boot` would.
        let mut blank =
            MfsVolume::format(MfsVolume::FLOPPY_400K, "Clone", ImageFormat::Raw).unwrap();
        blank.set_boot_blocks(boot.as_slice().try_into().unwrap()).unwrap();
        assert_eq!(blank.boot_blocks(), &boot[..]);
        // ...and the donor is unchanged, bytes and all.
        assert_eq!(vol.boot_blocks(), &boot[..]);
        let saved = save_bytes(&mut vol);
        assert_eq!(&saved[84..84 + 1024], &boot[..]);
    }

    #[test]
    fn save_path_and_open_path_round_trip() {
        let dir = std::env::temp_dir().join(format!("macfs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scratch.image");

        let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "OnDisk", ImageFormat::DiskCopy42).unwrap();
        vol.create_file("Hello", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Hello", Fork::Data, b"world").unwrap();
        vol.save_path(&path).unwrap();

        let back = MfsVolume::open_path(&path).unwrap();
        assert_eq!(back.info().name, "OnDisk");
        assert_eq!(back.read_fork("Hello", Fork::Data).unwrap(), b"world");

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}
