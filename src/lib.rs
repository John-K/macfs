//! Read and write MFS (Macintosh File System) floppy disk images.
//!
//! MFS is the flat filesystem the original 1984 Macintosh used on its 400K
//! single-sided floppies. This crate reads and writes those volumes — as bare
//! sector images or inside a [DiskCopy 4.2][ImageFormat::DiskCopy42] container,
//! autodetected on open and written back in the same shape. On-disk structure
//! layouts are described declaratively with [binrw](https://docs.rs/binrw),
//! which is the crate's only dependency.
//!
//! Volumes are handled whole: [`MfsVolume::open`] pulls the entire image into
//! memory, every operation works on that copy, and one [`MfsVolume::save_to`]
//! call re-serializes it. Nothing touches the underlying stream in between, so
//! a failed operation can never leave a half-written volume behind.
//!
//! Files have the two classic Mac forks — [`Fork::Data`] and
//! [`Fork::Resource`] — and names are stored as MacRoman, so lookups are
//! case-insensitive and names round-trip through [`String`].
//!
//! Anything this crate does not interpret is preserved verbatim: boot blocks,
//! reserved MDB and directory fields, and DiskCopy 4.2 tags. Opening an image
//! and saving it again without changing anything reproduces the original bytes
//! exactly.
//!
//! # Example
//!
//! ```
//! use macfs::{Fork, ImageFormat, MfsVolume};
//! use std::io::Cursor;
//!
//! // Format a blank 400K volume in memory and put a file on it.
//! let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "My Disk", ImageFormat::Raw)?;
//! vol.create_file("Read Me", *b"TEXT", *b"MACA")?;
//! vol.write_fork("Read Me", Fork::Data, b"hello from 1984")?;
//! vol.write_fork("Read Me", Fork::Resource, &[0xAB; 300])?;
//!
//! let mut image = Cursor::new(Vec::new());
//! vol.save_to(&mut image)?;
//!
//! // Read it back. Name lookup is case-insensitive, as on a real Mac.
//! let disk = MfsVolume::open(Cursor::new(image.into_inner()))?;
//! assert_eq!(disk.info().name, "My Disk");
//! assert_eq!(disk.files().count(), 1);
//! assert_eq!(disk.read_fork("read me", Fork::Data)?, b"hello from 1984");
//! assert_eq!(disk.read_fork("READ ME", Fork::Resource)?, vec![0xAB; 300]);
//! assert!(disk.check().is_empty());
//! # Ok::<(), macfs::MfsError>(())
//! ```

mod blockmap;
mod dc42;
mod dir;
mod error;
mod macroman;
mod mdb;
mod mkfs;
mod timestamp;
mod util;
mod volume;

pub use error::{MfsError, Result};
pub use timestamp::MacTimestamp;
pub use volume::{FileEntry, Fork, ImageFormat, MfsVolume, VolumeInfo};
