# Testing macfs

## Test tiers

1. **Unit tests** — in each `src/*.rs` module; no external files. `cargo test`.
2. **Round-trip integration** — `tests/roundtrip.rs`; exercises mkfs → mutate →
   save → reopen entirely in memory. No external files.
3. **Golden-image integration** — `tests/golden.rs`; runs against real MFS disk
   images in `tests/images/`. Each test **skips silently** (with an eprintln
   note) when its image is absent, so a fresh clone stays green.
4. **Manual smoke test** — mount an image this library wrote in an emulator
   (below). Not automated.

## Golden images

`tests/images/` is git-ignored: the Apple system disks are copyrighted, so they
are downloaded on demand rather than committed. Populate the directory with:

```sh
sh scripts/fetch-test-images.sh
```

The script is idempotent, never hard-fails on an unreachable source, and needs
`curl` + `unzip` (always present on macOS). The System 1.1/2.0 disks
additionally need `unar` (`brew install unar`) to unpack StuffIt archives;
without it those two images are skipped and their tests skip too.

| File | Bytes | What it is | Source |
|------|-------|------------|--------|
| `Sample.img` | 419,284 | Apple's MFSLives sample volume. DiskCopy 4.2 **despite the .img name** (84-byte header + 409,600 data + 9,600 tags), MFS inside. | [MFSLives repo](https://github.com/sp1ke23/MFSLives) (Apple sample code) |
| `gryphel-mfs400k.image` | 419,284 | Blank Apple-formatted 400K MFS volume, DC42-wrapped. The **mkfs oracle**: our `format()` geometry is compared field-by-field against its MDB. | [Mini vMac blanks-1.1.zip](https://www.gryphel.com/c/minivmac/extras/blanks/), inner `dc42/mfs400K.zip` |
| `gryphel-mfs800k.image` | 838,484 | Blank 800K MFS volume, DC42-wrapped (819,200 data + 19,200 tags). | same, inner `dc42/mfs800K.zip` |
| `Finder 1.0.image` | 419,284 | Original January 1984 System/Finder 1.0 boot disk, DC42. Real Finder-written MFS structures. | [earlymacintosh.org](https://www.earlymacintosh.org/disk_images.html) (`Finder 1.0.zip`) |
| `1.1 System Disk.image` | 419,284 | System 1.1 (May 1984) boot disk, DC42. Downloaded as zip → StuffIt → image; needs `unar`. | earlymacintosh.org (`1.1 System Disk.sit`) |
| `2.0 System Disk.image` | 419,284 | System 2.0 (April 1985) boot disk, DC42. Needs `unar`. | earlymacintosh.org (`2.0 System Disk.sit`) |

License note: the gryphel blanks are freely distributed; the Apple system
software images are abandonware whose copyright still belongs to Apple —
which is why they are fetched to a git-ignored directory instead of being
redistributed with this repository. Do not commit them.

All six images are DiskCopy 4.2 containers (magic `0x0100` at offset 82; MFS
signature `0xD2D7` at offset 84 + 1024). The library's raw-image path is
covered by the round-trip suite and by DC42-unwrapping these images in tests.

## What the golden tests assert

- DC42 checksums verify on open (`Dc42Image::parse` fails loudly otherwise).
- MDB invariants and expected volume names/file counts.
- Every file's data and resource fork on every image reads without
  `CorruptVolume`.
- **Byte-identical round trip**: `open` → `save` with no mutation reproduces
  the input file exactly, DC42 header and tag data included.
- **Write safety**: after renaming one file on an in-memory copy, every other
  file's fork bytes hash identically to before.
- mkfs geometry matches the Apple-formatted gryphel blank.

## Manual emulator smoke test

Automated tests prove self-consistency; this proves a real Mac accepts what we
write. With [Mini vMac](https://www.gryphel.com/c/minivmac/) (Macintosh Plus
ROM) or a Mac 128K variant:

```sh
cargo run --bin mfs -- mkfs smoke.image --dc42 --name Smoke
printf 'Hello from macfs\n' > hello.txt
cargo run --bin mfs -- add smoke.image hello.txt --name "Hello" --type TEXT --creator ttxt
cargo run --bin mfs -- check smoke.image
```

To make the disk *bootable* instead of a plain data disk, clone the boot
blocks (sectors 0–1, Apple's 68K boot code — which is why mkfs can't generate
them) from a real system disk, then copy the System and Finder files over:

```sh
cargo run --bin mfs -- bootblocks "tests/images/1.1 System Disk.image" --export bb.bin
cargo run --bin mfs -- bootblocks smoke.image --import bb.bin
for f in System Finder; do
  cargo run --bin mfs -- extract "tests/images/1.1 System Disk.image" "$f" -o "$f.data"
  cargo run --bin mfs -- extract "tests/images/1.1 System Disk.image" "$f" --rsrc -o "$f.rsrc"
done
cargo run --bin mfs -- add smoke.image System.data --name System --type zsys --creator MACS --rsrc System.rsrc
cargo run --bin mfs -- add smoke.image Finder.data --name Finder --type FNDR --creator MACS --rsrc Finder.rsrc
cargo run --bin mfs -- info smoke.image     # should report: bootable: yes
```

Then boot Mini vMac (Mac 128K variant for System 1.x) directly from
`smoke.image`.

1. Boot Mini vMac from a System disk image (e.g. `1.1 System Disk.image` or a
   System 6 boot floppy).
2. Drag `smoke.image` onto the Mini vMac window.
3. The volume "Smoke" should mount on the desktop with no "This disk is
   damaged" or "unreadable" dialog.
4. Open it; "Hello" should be listed, openable in TeachText on System 6
   (System 1.x has no TeachText on the bare system disk — the file listing
   itself is the check there).
5. Optionally copy a file onto the volume in the emulator, unmount, and run
   `cargo run --bin mfs -- check smoke.image` again: the volume the emulator
   modified must still parse cleanly and list the new file.
