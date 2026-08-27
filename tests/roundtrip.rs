//! Black-box round-trip tests: format a volume, mutate it through the public
//! API, serialize it, and then re-open the resulting bytes and assert on *that*
//! volume rather than on the one still in memory.
//!
//! Reopening is the whole point. An in-memory assertion only proves the
//! bookkeeping is self-consistent; reading the saved image back proves the
//! bytes on the disk say the same thing.
//!
//! No external files are needed — everything here starts from
//! [`MfsVolume::format`].

mod common;

use std::io::Cursor;

use macfs::{Fork, ImageFormat, MacTimestamp, MfsError, MfsVolume};

/// Serialize a volume and hand back the bytes.
fn save(vol: &mut MfsVolume) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    vol.save_to(&mut out).expect("save");
    out.into_inner()
}

/// Serialize a volume and open the result — the shape almost every test here
/// takes.
fn round_trip(vol: &mut MfsVolume) -> MfsVolume {
    MfsVolume::open(Cursor::new(save(vol))).expect("reopen the saved image")
}

/// A fresh volume, panicking on the geometry errors that cannot happen here.
fn blank(size: u32, name: &str, format: ImageFormat) -> MfsVolume {
    MfsVolume::format(size, name, format).expect("format")
}

/// Create a file and fill its data fork in one step.
fn put(vol: &mut MfsVolume, name: &str, data: &[u8]) {
    vol.create_file(name, *b"TEXT", *b"MACA").expect("create_file");
    vol.write_fork(name, Fork::Data, data).expect("write_fork");
}

/// Assert a volume reports no inconsistencies, printing them if it does.
fn assert_consistent(vol: &MfsVolume, what: &str) {
    let problems = vol.check();
    assert!(problems.is_empty(), "{what}: {problems:?}");
}

/// The three container/geometry combinations everything should behave the same
/// way in.
const SHAPES: [(u32, ImageFormat, &str); 3] = [
    (MfsVolume::FLOPPY_400K, ImageFormat::Raw, "400K raw"),
    (MfsVolume::FLOPPY_800K, ImageFormat::Raw, "800K raw"),
    (MfsVolume::FLOPPY_400K, ImageFormat::DiskCopy42, "400K DiskCopy 4.2"),
];

// ---------------------------------------------------------------- formatting

#[test]
fn mkfs_400k_is_openable() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Fresh Disk", ImageFormat::Raw);
    let image = save(&mut vol);
    assert_eq!(image.len(), MfsVolume::FLOPPY_400K as usize);

    let back = MfsVolume::open(Cursor::new(image)).expect("a fresh volume must reopen");
    let info = back.info();
    assert_eq!(info.name, "Fresh Disk");
    assert_eq!(info.format, ImageFormat::Raw);
    assert_eq!(info.file_count, 0);
    assert_eq!(back.files().count(), 0);
    assert_eq!(info.alloc_block_size, 1024);
    assert_eq!(info.total_blocks, 391);
    assert_eq!(info.free_blocks, 391);
    // A plain data disk: no boot code.
    assert_eq!(back.boot_blocks().len(), 1024);
    assert!(back.boot_blocks().iter().all(|&b| b == 0));
    assert_consistent(&back, "fresh 400K");
}

/// A freshly formatted volume's counters must agree with its own map.
///
/// Note the invariant that is *not* asserted: `drAlBlSt == drDirSt +
/// drDirLen`. The 800K geometry leaves slack between the end of the directory
/// and the first allocation block, so the counters — free blocks, file count,
/// and `check()` — are the real invariant.
#[test]
fn mkfs_matches_mdb_invariants() {
    for (size, format, label) in SHAPES {
        let mut vol = blank(size, "Invariants", format);
        let info = vol.info();
        assert_eq!(info.file_count, 0, "{label}");
        assert_eq!(info.free_blocks, info.total_blocks, "{label}: nothing is allocated yet");
        assert!(info.alloc_block_size >= 512, "{label}");
        assert_eq!(info.alloc_block_size % 512, 0, "{label}: not a whole number of sectors");
        // Every allocation block must fit inside the image.
        let alloc_bytes = info.total_blocks as u64 * info.alloc_block_size as u64;
        assert!(alloc_bytes < size as u64, "{label}: {alloc_bytes} bytes of blocks in {size}");
        // ...and what is left over is only the boot blocks, the MDB, the
        // allocation block map and the directory, plus under one block of
        // slack: a couple of percent, never a meaningful slice of the disk.
        let overhead = size as u64 - alloc_bytes;
        assert!(
            overhead * 20 < size as u64,
            "{label}: {overhead} of {size} bytes are not allocation blocks"
        );
        assert_consistent(&vol, label);

        let back = round_trip(&mut vol);
        assert_eq!(back.info(), info, "{label}: info changed across save/reopen");
        assert_consistent(&back, label);
    }
}

// ------------------------------------------------------------------- writing

/// Fork lengths on and around the allocation block boundary.
///
/// The boundary is derived from `info().alloc_block_size`, not hard-coded: a
/// 400K volume allocates in 1024-byte blocks and an 800K one in 2048-byte
/// blocks, so a literal 1024 here would silently stop testing the boundary on
/// the larger disk.
#[test]
fn add_sizes_boundary() {
    for (size, format, label) in SHAPES {
        let mut vol = blank(size, "Boundary", format);
        let blk = vol.info().alloc_block_size as usize;
        let free_before = vol.info().free_blocks;
        let lengths = [0usize, 1, blk - 1, blk, blk + 1, 100_000];

        let cases: Vec<(String, Vec<u8>)> = lengths
            .iter()
            .map(|&n| (format!("size {n}"), common::random_bytes(0x1234_0000 + n as u64, n)))
            .collect();
        for (name, data) in &cases {
            put(&mut vol, name, data);
        }

        let expected_blocks: usize = lengths.iter().map(|&n| n.div_ceil(blk)).sum();
        assert_eq!(
            vol.info().free_blocks as usize,
            free_before as usize - expected_blocks,
            "{label}"
        );
        assert_consistent(&vol, label);

        let back = round_trip(&mut vol);
        assert_eq!(back.info().file_count as usize, cases.len(), "{label}");
        assert_consistent(&back, label);
        for (name, data) in &cases {
            assert_eq!(&back.read_fork(name, Fork::Data).unwrap(), data, "{label}: {name}");
            let e = back.file(name).unwrap();
            assert_eq!(e.data_len as usize, data.len(), "{label}: {name}");
            assert_eq!(
                e.data_alloc as usize,
                data.len().div_ceil(blk) * blk,
                "{label}: {name} allocated length"
            );
            // An empty fork owns no blocks at all.
            assert_eq!(e.data_alloc == 0, data.is_empty(), "{label}: {name}");
            assert_eq!(e.rsrc_len, 0, "{label}: {name}");
        }
    }
}

/// The two forks of a file are allocated and replaced entirely independently.
#[test]
fn resource_fork_independent() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Forks", ImageFormat::Raw);
    let blk = vol.info().alloc_block_size as usize;
    let data = common::random_bytes(1, 3 * blk + 7);
    let rsrc = common::random_bytes(2, 5 * blk - 7);

    vol.create_file("Both Forks", *b"APPL", *b"MACS").unwrap();
    // A brand new file has two empty forks.
    assert!(vol.read_fork("Both Forks", Fork::Data).unwrap().is_empty());
    assert!(vol.read_fork("Both Forks", Fork::Resource).unwrap().is_empty());

    vol.write_fork("Both Forks", Fork::Data, &data).unwrap();
    // Writing the data fork leaves the resource fork empty...
    assert!(vol.read_fork("Both Forks", Fork::Resource).unwrap().is_empty());
    vol.write_fork("Both Forks", Fork::Resource, &rsrc).unwrap();
    // ...and vice versa.
    assert_eq!(vol.read_fork("Both Forks", Fork::Data).unwrap(), data);

    let back = round_trip(&mut vol);
    assert_consistent(&back, "both forks");
    assert_eq!(back.read_fork("both forks", Fork::Data).unwrap(), data);
    assert_eq!(back.read_fork("BOTH FORKS", Fork::Resource).unwrap(), rsrc);
    let e = back.file("Both Forks").unwrap();
    assert_eq!(e.data_len as usize, data.len());
    assert_eq!(e.rsrc_len as usize, rsrc.len());
    assert_eq!(&e.type_code, b"APPL");
    assert_eq!(&e.creator, b"MACS");

    // Clearing one fork does not disturb the other.
    let mut vol = back;
    vol.write_fork("Both Forks", Fork::Resource, &[]).unwrap();
    let back = round_trip(&mut vol);
    assert_eq!(back.read_fork("Both Forks", Fork::Data).unwrap(), data);
    assert!(back.read_fork("Both Forks", Fork::Resource).unwrap().is_empty());
    assert_eq!(back.file("Both Forks").unwrap().rsrc_alloc, 0);
    assert_consistent(&back, "resource fork cleared");
}

/// Rewriting a fork smaller and then larger, with the free-block arithmetic
/// checked exactly at each step.
#[test]
fn overwrite_shrinks_and_grows() {
    for (size, format, label) in SHAPES {
        let mut vol = blank(size, "Overwrite", format);
        let blk = vol.info().alloc_block_size;
        let free = vol.info().free_blocks;

        let big = common::random_bytes(10, 40 * blk as usize);
        let small = common::random_bytes(11, blk as usize / 2);
        let bigger = common::random_bytes(12, 60 * blk as usize + 1);

        put(&mut vol, "Elastic", &big);
        assert_eq!(vol.info().free_blocks, free - 40, "{label}: initial write");

        vol.write_fork("Elastic", Fork::Data, &small).unwrap();
        assert_eq!(vol.info().free_blocks, free - 1, "{label}: shrunk");
        assert_eq!(vol.read_fork("Elastic", Fork::Data).unwrap(), small, "{label}");

        vol.write_fork("Elastic", Fork::Data, &bigger).unwrap();
        assert_eq!(vol.info().free_blocks, free - 61, "{label}: grown");

        let back = round_trip(&mut vol);
        assert_consistent(&back, label);
        assert_eq!(back.info().free_blocks, free - 61, "{label}");
        assert_eq!(back.read_fork("Elastic", Fork::Data).unwrap(), bigger, "{label}");
        assert_eq!(back.file("Elastic").unwrap().data_len as usize, bigger.len());
        assert_eq!(back.file("Elastic").unwrap().data_alloc, 61 * blk);

        // Emptying the fork releases every block it held.
        let mut vol = back;
        vol.write_fork("Elastic", Fork::Data, &[]).unwrap();
        let back = round_trip(&mut vol);
        assert_eq!(back.info().free_blocks, free, "{label}: emptied");
        assert_eq!(back.info().file_count, 1, "{label}: the file itself remains");
        assert_consistent(&back, label);
    }
}

/// Delete a file from the middle of the volume and refill the hole with
/// something too big to fit in it, forcing a non-contiguous block chain.
#[test]
fn fragmentation_chain() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Swiss Cheese", ImageFormat::Raw);
    let blk = vol.info().alloc_block_size as usize;
    let free = vol.info().free_blocks;

    let a = common::random_bytes(0xA, 3 * blk);
    let b = common::random_bytes(0xB, 5 * blk);
    let c = common::random_bytes(0xC, 3 * blk);
    // Larger than the hole B leaves behind, so it must straddle C.
    let d = common::random_bytes(0xD, 9 * blk);

    put(&mut vol, "A", &a);
    put(&mut vol, "B", &b);
    put(&mut vol, "C", &c);
    assert_eq!(vol.info().free_blocks, free - 11);

    vol.delete_file("B").unwrap();
    assert_eq!(vol.info().free_blocks, free - 6, "the hole is open");

    put(&mut vol, "D", &d);
    assert_eq!(vol.info().free_blocks, free - 15);
    assert_consistent(&vol, "fragmented");

    let back = round_trip(&mut vol);
    assert_consistent(&back, "fragmented, reopened");
    assert_eq!(back.info().file_count, 3);
    assert_eq!(back.info().free_blocks, free - 15);
    for (name, want) in [("A", &a), ("C", &c), ("D", &d)] {
        let got = back.read_fork(name, Fork::Data).unwrap();
        assert_eq!(common::crc32(&got), common::crc32(want), "{name}: content changed");
        assert_eq!(&got, want, "{name}");
    }
    assert!(matches!(back.file("B"), Err(MfsError::FileNotFound(_))));

    // Whatever order the blocks ended up in, D still spans nine of them.
    assert_eq!(back.file("D").unwrap().data_alloc as usize, 9 * blk);
}

#[test]
fn delete_restores_free() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Recycler", ImageFormat::Raw);
    let blk = vol.info().alloc_block_size as usize;
    let free = vol.info().free_blocks;

    let keep = common::random_bytes(100, 7 * blk);
    put(&mut vol, "Keeper", &keep);
    vol.create_file("Doomed", *b"TEXT", *b"MACA").unwrap();
    vol.write_fork("Doomed", Fork::Data, &common::random_bytes(101, 11 * blk)).unwrap();
    vol.write_fork("Doomed", Fork::Resource, &common::random_bytes(102, 4 * blk)).unwrap();
    assert_eq!(vol.info().free_blocks, free - 22);
    assert_eq!(vol.info().file_count, 2);

    // Deleting releases both forks at once, and the lookup is case-insensitive.
    vol.delete_file("DOOMED").unwrap();
    assert_eq!(vol.info().free_blocks, free - 7);
    assert_eq!(vol.info().file_count, 1);

    let back = round_trip(&mut vol);
    assert_consistent(&back, "after delete");
    assert_eq!(back.info().free_blocks, free - 7);
    assert_eq!(back.info().file_count, 1);
    assert_eq!(back.files().count(), 1);
    assert_eq!(back.read_fork("Keeper", Fork::Data).unwrap(), keep);
    assert!(matches!(back.file("Doomed"), Err(MfsError::FileNotFound(_))));
    assert!(matches!(
        back.read_fork("Doomed", Fork::Data),
        Err(MfsError::FileNotFound(_))
    ));
}

/// A rename that lengthens an entry forces the directory region to be repacked,
/// which moves every entry after it.
#[test]
fn rename_repacks_directory() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Renames", ImageFormat::Raw);
    let blk = vol.info().alloc_block_size as usize;

    let files: Vec<(String, Vec<u8>)> = (0..12)
        .map(|i| (format!("f{i}"), common::random_bytes(200 + i, (i as usize % 4 + 1) * blk)))
        .collect();
    for (name, data) in &files {
        put(&mut vol, name, data);
    }
    let free_before = vol.info().free_blocks;

    // 63 bytes is the longest name MFS can hold; growing "f5" to it is the
    // worst case for the packer.
    let long = "f5 ".to_string() + &"x".repeat(60);
    assert_eq!(long.len(), 63);
    vol.rename_file("F5", &long).unwrap();
    assert_consistent(&vol, "after rename");

    let back = round_trip(&mut vol);
    assert_consistent(&back, "after rename, reopened");
    assert_eq!(back.info().file_count as usize, files.len());
    // A rename touches no allocation blocks.
    assert_eq!(back.info().free_blocks, free_before);

    for (i, (name, data)) in files.iter().enumerate() {
        let now: &str = if i == 5 { &long } else { name };
        assert_eq!(&back.read_fork(now, Fork::Data).unwrap(), data, "{now}");
        assert_eq!(back.file(now).unwrap().name, now, "{now}");
        assert_eq!(back.file(now).unwrap().file_num, i as u32 + 1, "{now}");
    }
    assert!(matches!(back.file("f5"), Err(MfsError::FileNotFound(_))));

    // Renaming back shortens the entry again, and everything still survives.
    let mut vol = back;
    vol.rename_file(&long, "f5").unwrap();
    let back = round_trip(&mut vol);
    assert_consistent(&back, "renamed back");
    for (name, data) in &files {
        assert_eq!(&back.read_fork(name, Fork::Data).unwrap(), data, "{name}");
    }
}

// -------------------------------------------------------------------- errors

/// Filling the volume must fail cleanly: the failed call changes nothing.
#[test]
fn volume_full_is_clean() {
    for (size, format, label) in SHAPES {
        let mut vol = blank(size, "Cramped", format);
        let blk = vol.info().alloc_block_size as usize;
        let total = vol.info().total_blocks as usize;

        let survivor = common::random_bytes(300, 3 * blk);
        put(&mut vol, "Survivor", &survivor);

        // Fill every remaining block, then ask for one more byte.
        let remaining = vol.info().free_blocks as usize;
        vol.create_file("Filler", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Filler", Fork::Data, &common::random_bytes(301, remaining * blk))
            .unwrap();
        assert_eq!(vol.info().free_blocks, 0, "{label}");

        vol.create_file("One More", *b"TEXT", *b"MACA").unwrap();
        let before = save(&mut vol);
        match vol.write_fork("One More", Fork::Data, &[0xFF]) {
            Err(MfsError::VolumeFull { needed_blocks, free_blocks }) => {
                assert_eq!(needed_blocks, 1, "{label}");
                assert_eq!(free_blocks, 0, "{label}");
            }
            other => panic!("{label}: expected VolumeFull, got {other:?}"),
        }
        // The failed call left the volume untouched, byte for byte.
        assert_eq!(vol.info().free_blocks, 0, "{label}");
        assert_eq!(save(&mut vol), before, "{label}: the failed write changed the image");
        assert_consistent(&vol, label);

        // Growing an existing fork past the end fails the same way, and the
        // blocks it already owns are counted as available to it.
        match vol.write_fork("Survivor", Fork::Data, &vec![0u8; (total + 1) * blk]) {
            Err(MfsError::VolumeFull { needed_blocks, free_blocks }) => {
                assert_eq!(needed_blocks as usize, total + 1, "{label}");
                assert_eq!(free_blocks, 3, "{label}: the fork's own blocks count");
            }
            other => panic!("{label}: expected VolumeFull, got {other:?}"),
        }

        let back = round_trip(&mut vol);
        assert_consistent(&back, label);
        assert_eq!(back.info().free_blocks, 0, "{label}");
        assert_eq!(back.read_fork("Survivor", Fork::Data).unwrap(), survivor, "{label}");
        assert!(back.read_fork("One More", Fork::Data).unwrap().is_empty(), "{label}");
    }
}

/// Filling the directory must fail cleanly too — and at `create_file` time,
/// not at save time.
#[test]
fn directory_full_is_clean() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Crowded", ImageFormat::Raw);
    let mut created = 0u32;
    loop {
        match vol.create_file(&format!("f{created}"), *b"TEXT", *b"MACA") {
            Ok(()) => created += 1,
            Err(MfsError::DirectoryFull) => break,
            Err(e) => panic!("unexpected error after {created} files: {e}"),
        }
        assert!(created < 10_000, "the directory never filled up");
    }
    assert!(created > 50, "only {created} files fit, which looks wrong");
    assert_eq!(vol.info().file_count as u32, created);
    assert_consistent(&vol, "full directory");

    // The rejected create changed nothing at all.
    let before = save(&mut vol);
    assert!(matches!(
        vol.create_file("one too many", *b"TEXT", *b"MACA"),
        Err(MfsError::DirectoryFull)
    ));
    assert_eq!(vol.info().file_count as u32, created);
    assert_eq!(save(&mut vol), before, "the failed create changed the image");

    // A rename that would grow an entry past the end is refused the same way.
    assert!(matches!(
        vol.rename_file("f0", &"n".repeat(63)),
        Err(MfsError::DirectoryFull)
    ));
    assert_eq!(vol.file("f0").unwrap().name, "f0");

    let back = MfsVolume::open(Cursor::new(before)).expect("a full directory must reopen");
    assert_consistent(&back, "full directory, reopened");
    assert_eq!(back.info().file_count as u32, created);
    assert_eq!(back.files().count() as u32, created);
    for i in 0..created {
        assert_eq!(back.file(&format!("f{i}")).unwrap().file_num, i + 1);
    }

    // Deleting one makes room for exactly one more.
    let mut vol = back;
    vol.delete_file("f0").unwrap();
    vol.create_file("f0", *b"TEXT", *b"MACA").unwrap();
    assert!(matches!(
        vol.create_file("and another", *b"TEXT", *b"MACA"),
        Err(MfsError::DirectoryFull)
    ));
}

/// Names collide case-insensitively, as they do on a real Mac.
#[test]
fn duplicate_name_rejected() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Names", ImageFormat::Raw);
    vol.create_file("readme", *b"TEXT", *b"MACA").unwrap();
    assert!(matches!(
        vol.create_file("README", *b"TEXT", *b"MACA"),
        Err(MfsError::FileExists(_))
    ));
    assert!(matches!(
        vol.create_file("ReAdMe", *b"TEXT", *b"MACA"),
        Err(MfsError::FileExists(_))
    ));
    assert_eq!(vol.info().file_count, 1);

    // Renaming onto an existing name is refused too.
    vol.create_file("Notes", *b"TEXT", *b"MACA").unwrap();
    assert!(matches!(
        vol.rename_file("Notes", "README"),
        Err(MfsError::FileExists(_))
    ));
    assert_eq!(vol.file("Notes").unwrap().name, "Notes");
    // ...but changing a file's own name's case is not a collision with itself.
    vol.rename_file("Notes", "NOTES").unwrap();
    assert_eq!(vol.file("notes").unwrap().name, "NOTES");

    let back = round_trip(&mut vol);
    assert_consistent(&back, "duplicate names");
    assert_eq!(back.info().file_count, 2);
    assert_eq!(back.file("readme").unwrap().name, "readme");
    assert_eq!(back.file("Notes").unwrap().name, "NOTES");
}

// ------------------------------------------------------------- file numbers

/// `drNxtFNum` only ever counts up: a deleted file's number is never reissued.
#[test]
fn nxt_fnum_monotone() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Numbers", ImageFormat::Raw);
    let mut issued: Vec<u32> = Vec::new();

    for i in 0..6 {
        let name = format!("gen0-{i}");
        vol.create_file(&name, *b"TEXT", *b"MACA").unwrap();
        issued.push(vol.file(&name).unwrap().file_num);
    }
    assert_eq!(issued, vec![1, 2, 3, 4, 5, 6]);

    // Delete from the middle and the end, then create again.
    vol.delete_file("gen0-1").unwrap();
    vol.delete_file("gen0-5").unwrap();
    for i in 0..3 {
        let name = format!("gen1-{i}");
        vol.create_file(&name, *b"TEXT", *b"MACA").unwrap();
        issued.push(vol.file(&name).unwrap().file_num);
    }
    assert_eq!(issued, vec![1, 2, 3, 4, 5, 6, 7, 8, 9], "numbers must not be reused");

    // The counter survives save/reopen and keeps counting from where it was.
    let mut back = round_trip(&mut vol);
    assert_consistent(&back, "file numbers");
    back.create_file("gen2-0", *b"TEXT", *b"MACA").unwrap();
    assert_eq!(back.file("gen2-0").unwrap().file_num, 10);

    // No live file shares a number, and none was recycled.
    let mut live: Vec<u32> = back.files().map(|f| f.file_num).collect();
    live.sort_unstable();
    let unique_len = {
        let mut d = live.clone();
        d.dedup();
        d.len()
    };
    assert_eq!(unique_len, live.len(), "duplicate file numbers: {live:?}");
    assert!(!live.contains(&2), "gen0-1's number was reissued");
    assert!(!live.contains(&6), "gen0-5's number was reissued");
}

// ------------------------------------------------------- containers & bytes

/// The DiskCopy 4.2 container survives the full mutation cycle, and forcing
/// the format explicitly agrees with autodetection.
#[test]
fn dc42_roundtrip() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Copy Disk", ImageFormat::DiskCopy42);
    let blk = vol.info().alloc_block_size as usize;

    let alpha = common::random_bytes(400, 6 * blk + 3);
    let beta = common::random_bytes(401, 2 * blk);
    let gamma = common::random_bytes(402, 9 * blk - 1);

    put(&mut vol, "Alpha", &alpha);
    put(&mut vol, "Beta", &beta);
    put(&mut vol, "Gamma", &gamma);
    vol.write_fork("Beta", Fork::Resource, &common::random_bytes(403, 300)).unwrap();
    // Overwrite, then delete.
    vol.write_fork("Alpha", Fork::Data, &alpha[..blk]).unwrap();
    vol.write_fork("Alpha", Fork::Data, &alpha).unwrap();
    vol.delete_file("Beta").unwrap();

    let image = save(&mut vol);
    // 84-byte header + 400K of disk + 12 tag bytes per 512-byte sector.
    assert_eq!(image.len(), 84 + 409_600 + 800 * 12);

    let back = MfsVolume::open(Cursor::new(image.clone())).expect("autodetect DiskCopy 4.2");
    assert_eq!(back.info().format, ImageFormat::DiskCopy42);
    assert_eq!(back.info().name, "Copy Disk");
    assert_eq!(back.info().file_count, 2);
    assert_consistent(&back, "dc42");
    assert_eq!(back.read_fork("Alpha", Fork::Data).unwrap(), alpha);
    assert_eq!(back.read_fork("Gamma", Fork::Data).unwrap(), gamma);
    assert!(matches!(back.file("Beta"), Err(MfsError::FileNotFound(_))));

    // Naming the format explicitly gives the identical volume.
    let forced = MfsVolume::open_with_format(Cursor::new(image.clone()), ImageFormat::DiskCopy42)
        .expect("open_with_format");
    assert_eq!(forced.info(), back.info());
    assert_eq!(forced.read_fork("Alpha", Fork::Data).unwrap(), alpha);
    // ...and naming the wrong one is an error, not a wrong answer.
    assert!(MfsVolume::open_with_format(Cursor::new(image), ImageFormat::Raw).is_err());

    // Re-saving the reopened volume reproduces the same container exactly.
    let mut back = back;
    let again = save(&mut back);
    let mut once_more = MfsVolume::open(Cursor::new(again.clone())).unwrap();
    assert_eq!(save(&mut once_more), again, "the container is not stable");
}

/// Serializing is a pure function of the volume state: saving twice in a row,
/// with and without an intervening mutation, gives the same bytes.
#[test]
fn open_save_byte_identical() {
    for (size, format, label) in SHAPES {
        let mut vol = blank(size, "Stable", format);

        let first = save(&mut vol);
        assert_eq!(save(&mut vol), first, "{label}: a fresh volume is not stable");

        // Reopening and re-saving reproduces the image too.
        let mut back = MfsVolume::open(Cursor::new(first.clone())).unwrap();
        assert_eq!(save(&mut back), first, "{label}: open/save is not byte-identical");

        // Now mutate, and check the same property of the mutated volume.
        vol.create_file("Payload", *b"TEXT", *b"MACA").unwrap();
        vol.write_fork("Payload", Fork::Data, &common::random_bytes(500, 20_000)).unwrap();
        vol.rename_volume("Stable Still").unwrap();
        vol.set_locked("Payload", true).unwrap();

        let mutated = save(&mut vol);
        assert_ne!(mutated, first, "{label}: the mutation did not reach the image");
        assert_eq!(save(&mut vol), mutated, "{label}: a mutated volume is not stable");

        let mut back = MfsVolume::open(Cursor::new(mutated.clone())).unwrap();
        assert_eq!(save(&mut back), mutated, "{label}: mutated open/save differs");
        assert_eq!(back.info().name, "Stable Still", "{label}");
        assert!(back.file("Payload").unwrap().locked, "{label}");
        assert_consistent(&back, label);
    }
}

/// Boot code is stored outside the filesystem and must round-trip verbatim.
#[test]
fn boot_blocks_roundtrip() {
    for (size, format, label) in SHAPES {
        let mut vol = blank(size, "Bootable", format);
        assert!(vol.boot_blocks().iter().all(|&b| b == 0), "{label}: not blank");

        let mut code = [0u8; 1024];
        code.copy_from_slice(&common::random_bytes(0xB007, 1024));
        // Make it look like real boot code: 'LK' is what the ROM checks for.
        code[..2].copy_from_slice(b"LK");
        vol.set_boot_blocks(&code).unwrap();
        assert_eq!(vol.boot_blocks(), code, "{label}");

        // Installing boot code does not disturb the filesystem.
        let payload = common::random_bytes(0xB008, 5_000);
        put(&mut vol, "Payload", &payload);

        let back = round_trip(&mut vol);
        assert_consistent(&back, label);
        assert_eq!(back.boot_blocks(), code, "{label}: boot blocks changed");
        assert_eq!(&back.boot_blocks()[..2], b"LK", "{label}");
        assert_eq!(back.read_fork("Payload", Fork::Data).unwrap(), payload, "{label}");
    }
}

/// A locked file refuses to be written or deleted until it is unlocked.
#[test]
fn locked_files_are_protected() {
    let mut vol = blank(MfsVolume::FLOPPY_400K, "Locks", ImageFormat::Raw);
    let precious = common::random_bytes(600, 4_000);
    put(&mut vol, "Precious", &precious);
    vol.set_times("Precious", Some(MacTimestamp(1_000_000)), Some(MacTimestamp(2_000_000)))
        .unwrap();
    vol.set_locked("Precious", true).unwrap();
    let free = vol.info().free_blocks;

    // The lock survives a save/reopen, and it still bites afterwards.
    let mut back = round_trip(&mut vol);
    let e = back.file("Precious").unwrap();
    assert!(e.locked);
    assert_eq!(e.created, MacTimestamp(1_000_000));
    assert_eq!(e.modified, MacTimestamp(2_000_000));

    assert!(matches!(back.delete_file("Precious"), Err(MfsError::FileLocked(_))));
    assert!(matches!(
        back.write_fork("precious", Fork::Data, b"nope"),
        Err(MfsError::FileLocked(_))
    ));
    assert!(matches!(
        back.write_fork("Precious", Fork::Resource, b"nope"),
        Err(MfsError::FileLocked(_))
    ));
    // Nothing was lost or freed by the refusals.
    assert_eq!(back.read_fork("Precious", Fork::Data).unwrap(), precious);
    assert_eq!(back.info().free_blocks, free);
    assert_eq!(back.info().file_count, 1);

    // Unlocking restores both operations.
    back.set_locked("PRECIOUS", false).unwrap();
    assert!(!back.file("Precious").unwrap().locked);
    back.write_fork("Precious", Fork::Data, b"replaced").unwrap();
    back.delete_file("Precious").unwrap();

    let back = round_trip(&mut back);
    assert_consistent(&back, "after unlock");
    assert_eq!(back.info().file_count, 0);
    assert_eq!(back.info().free_blocks, back.info().total_blocks);
}
