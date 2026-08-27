//! Black-box tests against real, historical MFS disk images.
//!
//! The images are Apple-copyrighted and are never committed; `scripts/
//! fetch-test-images.sh` downloads them into `tests/images/`. Every test here
//! begins by asking [`common::image_path`] for its image and returns quietly
//! when it is absent, so a fresh clone still runs a green suite — it just runs
//! a smaller one.
//!
//! The values asserted below were discovered by reading the real images and
//! are then pinned verbatim. That is the point: a refactor that changes how a
//! 1984 disk is interpreted has to change these numbers too, deliberately.

mod common;

use std::io::Cursor;

use macfs::{Fork, ImageFormat, MfsVolume};

/// Every image the suite knows about. All six are DiskCopy 4.2 containers.
const GOLDEN: [&str; 6] = [
    "Sample.img",
    "Finder 1.0.image",
    "1.1 System Disk.image",
    "2.0 System Disk.image",
    "gryphel-mfs400k.image",
    "gryphel-mfs800k.image",
];

/// `(name, data_len, rsrc_len)` for every file, sorted by name.
fn listing(vol: &MfsVolume) -> Vec<(String, u32, u32)> {
    let mut files: Vec<_> = vol
        .files()
        .map(|f| (f.name.clone(), f.data_len, f.rsrc_len))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn save_bytes(vol: &mut MfsVolume) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    vol.save_to(&mut out).expect("save");
    out.into_inner()
}

// ---------------------------------------------------------------- Sample.img

/// MFSLives' `Sample.img` — an MPW project disk — down to the last file.
#[test]
fn sample_img_contents() {
    let Some(p) = common::image_path("Sample.img") else { return };
    let vol = MfsVolume::open_path(&p).expect("open Sample.img");
    let info = vol.info();

    // Despite the `.img` extension it is a DiskCopy 4.2 container, with tags.
    assert_eq!(info.format, ImageFormat::DiskCopy42);
    assert_eq!(info.name, "Sample");
    assert_eq!(info.file_count, 10);
    assert_eq!(info.alloc_block_size, 1024);
    assert_eq!(info.total_blocks, 391);
    assert_eq!(info.free_blocks, 207);
    assert!(vol.check().is_empty(), "{:?}", vol.check());

    assert_eq!(
        listing(&vol),
        vec![
            ("CSillyBalls".to_string(), 0, 2417),
            ("CSillyBalls.make".to_string(), 1720, 382),
            ("DeskTop".to_string(), 0, 2540),
            ("PSillyBalls".to_string(), 0, 2479),
            ("PSillyBalls.make".to_string(), 1369, 382),
            ("SCN.003.SillyBalls".to_string(), 1294, 382),
            ("SillyBalls.c".to_string(), 8947, 382),
            ("SillyBalls.p".to_string(), 8573, 382),
            ("TN.002.Compatibility".to_string(), 13130, 833),
            ("TN.002.Compatibility.pdf".to_string(), 133827, 0),
        ]
    );

    // The largest data fork on the disk, extracted whole and fingerprinted.
    // Its 131 allocation blocks are the crate's longest real chain walk.
    let pdf = vol
        .read_fork("TN.002.Compatibility.pdf", Fork::Data)
        .expect("read the PDF");
    assert_eq!(pdf.len(), 133_827);
    assert_eq!(common::crc32(&pdf), 0xFB5B_B4D7);
    assert_eq!(&pdf[..5], b"%PDF-", "the extracted bytes really are a PDF");

    // A couple of resource-only files, to prove the other fork extracts too.
    let app = vol.read_fork("CSillyBalls", Fork::Resource).unwrap();
    assert_eq!(app.len(), 2417);
    assert_eq!(common::crc32(&app), 0x0862_1B3D);
    assert!(vol.read_fork("CSillyBalls", Fork::Data).unwrap().is_empty());

    // Type and creator survive the trip out of the directory.
    assert_eq!(&vol.file("csillyballs").unwrap().type_code, b"APPL");
    assert_eq!(&vol.file("SillyBalls.c").unwrap().creator, b"MPS ");
}

// --------------------------------------------------------------- system disks

/// The three real Apple disks: bootable, complete, and internally consistent.
#[test]
fn system_disks_bootable_and_readable() {
    // (image, volume name, file count, free blocks)
    let disks: [(&str, &str, u16, u16); 3] = [
        ("1.1 System Disk.image", "System Disk", 10, 105),
        ("2.0 System Disk.image", "System Disk", 10, 68),
        ("Finder 1.0.image", "Write/Paint", 11, 48),
    ];

    for (name, vol_name, file_count, free) in disks {
        let Some(p) = common::image_path(name) else { continue };
        let vol = MfsVolume::open_path(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        let info = vol.info();

        assert_eq!(info.format, ImageFormat::DiskCopy42, "{name}");
        assert_eq!(info.name, vol_name, "{name}");
        assert_eq!(info.file_count, file_count, "{name}");
        assert_eq!(info.alloc_block_size, 1024, "{name}");
        assert_eq!(info.total_blocks, 391, "{name}");
        assert_eq!(info.free_blocks, free, "{name}");

        // 'LK' — the boot block signature the 64K ROM looks for.
        assert_eq!(&vol.boot_blocks()[..2], b"LK", "{name} should be bootable");
        assert_eq!(vol.boot_blocks().len(), 1024, "{name}");

        // Every system disk carries a System file and a Finder, and both are
        // findable in any case, as they would be from the Mac's own Toolbox.
        for wanted in ["System", "Finder"] {
            let f = vol
                .file(&wanted.to_lowercase())
                .unwrap_or_else(|e| panic!("{name}: {wanted}: {e}"));
            assert_eq!(f.name, wanted, "{name}");
            assert_eq!(&f.type_code, if wanted == "System" { b"ZSYS" } else { b"FNDR" });
            // Both live in their resource forks, as classic Mac binaries do.
            assert!(f.rsrc_len > 40_000, "{name}: {wanted} resource fork is {}", f.rsrc_len);
        }

        // Every fork of every file reads back at exactly its logical length.
        for f in vol.files() {
            for (fork, want) in [(Fork::Data, f.data_len), (Fork::Resource, f.rsrc_len)] {
                let got = vol
                    .read_fork(&f.name, fork)
                    .unwrap_or_else(|e| panic!("{name}: {} {fork:?} fork: {e}", f.name));
                assert_eq!(got.len() as u32, want, "{name}: {} {fork:?} fork", f.name);
            }
        }

        // Apple's own disks pass the consistency check with nothing to report.
        assert!(vol.check().is_empty(), "{name}: {:?}", vol.check());
    }
}

// -------------------------------------------------------------- blank volumes

/// Our `format()` geometry against the blanks Apple's own Disk Init produced.
#[test]
fn blanks_match_mkfs() {
    // The blanks are not quite empty: each carries one invisible Finder
    // desktop database file — spelled differently on the two sizes, which is
    // exactly the sort of thing only a real image tells you.
    let blanks: [(&str, u32, &str, u32, u32, u16, u16); 2] = [
        ("gryphel-mfs400k.image", MfsVolume::FLOPPY_400K, "Desktop", 461, 1024, 391, 390),
        ("gryphel-mfs800k.image", MfsVolume::FLOPPY_800K, "DeskTop", 512, 2048, 392, 391),
    ];

    for (name, size, desktop, desktop_rsrc, blk_siz, total, free) in blanks {
        let Some(p) = common::image_path(name) else { continue };
        let vol = MfsVolume::open_path(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        let info = vol.info();

        assert_eq!(info.name, "Untitled", "{name}");
        assert_eq!(info.file_count, 1, "{name}");
        assert_eq!(
            listing(&vol),
            vec![(desktop.to_string(), 0, desktop_rsrc)],
            "{name}"
        );
        assert_eq!(&vol.file(desktop).unwrap().type_code, b"FNDR", "{name}");
        // A blank data disk has no boot code.
        assert!(vol.boot_blocks().iter().all(|&b| b == 0), "{name}");
        assert!(vol.check().is_empty(), "{name}: {:?}", vol.check());

        // The geometry our formatter computes must be Apple's, block for block.
        let ours = MfsVolume::format(size, "Untitled", ImageFormat::Raw).unwrap().info();
        assert_eq!(ours.alloc_block_size, info.alloc_block_size, "{name}: drAlBlkSiz");
        assert_eq!(ours.total_blocks, info.total_blocks, "{name}: drNmAlBlks");
        assert_eq!(ours.name, info.name, "{name}: drVN");
        assert_eq!(ours.alloc_block_size, blk_siz, "{name}");
        assert_eq!(ours.total_blocks, total, "{name}");
        // Ours has no Desktop file, so it starts with one more free block.
        assert_eq!(info.free_blocks, free, "{name}");
        assert_eq!(ours.free_blocks, total, "{name}");
    }
}

// ------------------------------------------------------------ preservation

/// The keystone: opening a real image and saving it must reproduce it exactly.
#[test]
fn byte_identical_all() {
    let mut checked = 0;
    for name in GOLDEN {
        let Some(p) = common::image_path(name) else { continue };
        let original = std::fs::read(&p).unwrap();

        let mut vol = MfsVolume::open(Cursor::new(original.clone()))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let saved = save_bytes(&mut vol);

        assert_eq!(saved.len(), original.len(), "{name}: image length changed");
        if saved != original {
            let at = saved.iter().zip(&original).position(|(a, b)| a != b).unwrap();
            panic!(
                "{name}: first difference at byte {at}: saved {:#04x}, original {:#04x}",
                saved[at], original[at]
            );
        }
        // Saving a second time changes nothing either.
        assert_eq!(save_bytes(&mut vol), original, "{name}: second save differs");
        checked += 1;
    }
    eprintln!("byte-identical open/save over {checked} golden image(s)");
}

/// DiskCopy 4.2 tags live past the disk data and have no MFS meaning, so
/// nothing in the crate would notice losing them — except the length.
#[test]
fn dc42_tag_preservation() {
    let Some(p) = common::image_path("Sample.img") else { return };
    let original = std::fs::read(&p).unwrap();
    // 84-byte header + 409,600 bytes of disk + 12 tag bytes per 512-byte sector.
    assert_eq!(original.len(), 419_284);
    assert_eq!(original.len(), 84 + 409_600 + 800 * 12);

    let mut vol = MfsVolume::open(Cursor::new(original.clone())).unwrap();
    let saved = save_bytes(&mut vol);
    assert_eq!(saved.len(), 419_284, "the tag region was dropped or resized");
    assert_eq!(saved[84 + 409_600..], original[84 + 409_600..], "tags changed");
    assert_eq!(saved, original);
}

// ------------------------------------------------------------- write safety

/// Mutating a real disk must leave every other file exactly where it was,
/// even though `save` repacks the whole directory region from scratch.
#[test]
fn write_safety_on_real_image() {
    let Some(p) = common::image_path("2.0 System Disk.image") else { return };
    let original = std::fs::read(&p).unwrap();
    let mut vol = MfsVolume::open(Cursor::new(original.clone())).unwrap();

    // Fingerprint every fork before touching anything.
    let before: Vec<(String, u32, u32, u32)> = vol
        .files()
        .map(|f| {
            let d = common::crc32(&vol.read_fork(&f.name, Fork::Data).unwrap());
            let r = common::crc32(&vol.read_fork(&f.name, Fork::Resource).unwrap());
            (f.name.clone(), f.file_num, d, r)
        })
        .collect();
    let boot = vol.boot_blocks().to_vec();
    let free_before = vol.info().free_blocks;
    let count_before = vol.info().file_count;

    // Rename to a longer name — this grows the directory entry and forces
    // every entry after it to move.
    vol.rename_file("Note Pad File", "Note Pad File (renamed by macfs)").unwrap();

    // Add ten kilobytes of noise and then take it away again.
    let noise = common::random_bytes(0x5A5A_1984, 10 * 1024);
    vol.create_file("Scratch", *b"TEXT", *b"macf").unwrap();
    vol.write_fork("Scratch", Fork::Data, &noise).unwrap();
    assert_eq!(vol.info().free_blocks, free_before - 10);
    assert_eq!(vol.read_fork("scratch", Fork::Data).unwrap(), noise);
    vol.delete_file("Scratch").unwrap();
    assert_eq!(vol.info().free_blocks, free_before);

    let back = MfsVolume::open(Cursor::new(save_bytes(&mut vol))).unwrap();
    assert!(back.check().is_empty(), "{:?}", back.check());
    assert_eq!(back.info().file_count, count_before);
    assert_eq!(back.info().free_blocks, free_before);
    assert_eq!(back.info().name, "System Disk");
    // Still bootable: the boot blocks are outside the filesystem and untouched.
    assert_eq!(back.boot_blocks(), &boot[..]);
    assert_eq!(&back.boot_blocks()[..2], b"LK");

    for (name, fnum, data_crc, rsrc_crc) in &before {
        let now_name = if name == "Note Pad File" {
            "Note Pad File (renamed by macfs)"
        } else {
            name
        };
        let e = back
            .file(now_name)
            .unwrap_or_else(|e| panic!("{now_name} vanished: {e}"));
        assert_eq!(e.file_num, *fnum, "{now_name}: file number changed");
        assert_eq!(
            common::crc32(&back.read_fork(now_name, Fork::Data).unwrap()),
            *data_crc,
            "{now_name}: data fork changed"
        );
        assert_eq!(
            common::crc32(&back.read_fork(now_name, Fork::Resource).unwrap()),
            *rsrc_crc,
            "{now_name}: resource fork changed"
        );
    }
    // The renamed file is gone under its old name, and only under that one.
    assert!(back.file("Note Pad File").is_err());
    assert!(back.file("Scratch").is_err());
}
