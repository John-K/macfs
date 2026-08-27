# macfs

Read and write MFS (Macintosh File System) floppy disk images — the flat
filesystem the original 1984 Macintosh used on its 400K single-sided floppies —
with no dependencies beyond `std`.

Images may be bare sector dumps or DiskCopy 4.2 containers; the container is
autodetected on open and written back in the same shape. Anything the crate
does not interpret is preserved verbatim — boot blocks, reserved MDB and
directory fields, DiskCopy 4.2 tags — so opening an image and saving it again
without changes reproduces the original bytes exactly. HFS volumes are
detected and reported as unsupported.

## Library

```rust
use macfs::{Fork, ImageFormat, MfsVolume, Result};
use std::io::Cursor;

fn main() -> Result<()> {
    // Format a blank 400K volume in memory and put a file on it.
    let mut vol = MfsVolume::format(MfsVolume::FLOPPY_400K, "My Disk", ImageFormat::Raw)?;
    vol.create_file("Read Me", *b"TEXT", *b"MACA")?;
    vol.write_fork("Read Me", Fork::Data, b"hello from 1984")?;

    let mut image = Cursor::new(Vec::new());
    vol.save_to(&mut image)?;

    // Read it back. Name lookup is case-insensitive, as on a real Mac.
    let disk = MfsVolume::open(Cursor::new(image.into_inner()))?;
    assert_eq!(disk.read_fork("read me", Fork::Data)?, b"hello from 1984");
    Ok(())
}
```

Volumes are handled whole: `open` pulls the entire image into memory, every
operation works on that copy, and one `save_to` call re-serializes it — a
failed operation can never leave a half-written volume behind. Files have the
two classic Mac forks (data and resource), and names are stored as MacRoman.

## Command-line tool

The crate ships an `mfs` binary:

```text
mfs info       <image>
mfs ls         <image> [-l]
mfs cat        <image> <name> [--rsrc]
mfs extract    <image> <name> [--rsrc] [-o PATH]
mfs add        <image> <hostfile> [--name N] [--type XXXX] [--creator XXXX] [--rsrc HOSTFILE]
mfs rm         <image> <name> [--force]
mfs mv         <image> <old> <new>
mfs mkfs       <image> [--size 400k|800k|BYTES] [--name NAME] [--dc42] [--force]
mfs check      <image>
mfs bootblocks <image> [--export FILE | --import FILE]
```

## Testing

The unit and round-trip suites are self-contained. The golden tests
additionally verify byte-identical open/save against real Apple system disks,
which are copyrighted and not distributed with the crate; run
`scripts/fetch-test-images.sh` to download them into `tests/images/`, and set
`MACFS_REQUIRE_GOLDEN=1` to make their absence a test failure instead of a
skip. See `TESTING.md` for details.

## Resources

Format documentation:

- *Inside Macintosh*, Volume II, "The File Manager" (Apple, 1985) — the MFS
  on-disk structures: master directory block, file directory, and the 12-bit
  allocation block map. Field names in the source (`drSigWord`, `flFndrFlags`,
  …) follow its nomenclature.
- [MFSLives](https://github.com/sp1ke23/MFSLives) — Apple's MFS sample code, a
  reference implementation of the format.
- The DiskCopy 4.2 container layout and its checksum algorithm — including the
  undocumented but universal quirk that `tagChecksum` skips the first sector's
  12 tag bytes — were matched against real images produced by Apple's Disk
  Copy.

Golden test images (fetched by `scripts/fetch-test-images.sh`; provenance and
license notes in [TESTING.md](TESTING.md)):

- `Sample.img` from the [MFSLives](https://github.com/sp1ke23/MFSLives) repo —
  Apple's sample MFS volume.
- [Mini vMac blank disk images](https://www.gryphel.com/c/minivmac/extras/blanks/)
  (gryphel.com) — Apple-formatted blank 400K/800K volumes, used as the `mkfs`
  oracle.
- [earlymacintosh.org](https://www.earlymacintosh.org/disk_images.html) —
  Finder 1.0, System 1.1, and System 2.0 boot disks.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

---

Developed with the help of [Claude Code](https://claude.com/claude-code).
