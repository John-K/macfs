#!/bin/sh
# Downloads golden MFS disk images into tests/images/ (git-ignored).
# Integration tests in tests/golden.rs skip gracefully when a file is absent,
# so every source here is optional. Idempotent: existing files are kept.
# See TESTING.md for provenance and license notes.
set -u
cd "$(dirname "$0")/.." || exit 1
mkdir -p tests/images
cd tests/images || exit 1

TMPDIR_FETCH="$(mktemp -d)" || exit 1
trap 'rm -rf "$TMPDIR_FETCH"' EXIT

have() { [ -f "$1" ] && { echo "have  $1"; return 0; } || return 1; }

get() { # get <dest> <url>
    have "$1" && return 0
    if curl -fsSL "$2" -o "$1"; then
        echo "got   $1"
    else
        echo "FAIL  $1 ($2)"
        rm -f "$1"
    fi
}

# 1. MFSLives sample volume (Apple sample code; DiskCopy 4.2 with tag data
#    despite the .img name: 419284 = 84 + 409600 + 800*12).
get Sample.img "https://raw.githubusercontent.com/sp1ke23/MFSLives/master/Sample.img"

# 2. Mini vMac blank images (gryphel.com). The outer zip nests one zip per
#    image; the MFS blanks live in dc42/mfs{400,800}K.zip and are themselves
#    DiskCopy 4.2-wrapped (sig 0xD2D7 at offset 84+1024). They are the mkfs
#    oracle: Apple-formatted blank volumes.
if ! have gryphel-mfs400k.image || ! have gryphel-mfs800k.image; then
    if curl -fsSL "https://www.gryphel.com/d/minivmac/extras/blanks/blanks-1.1.zip" \
            -o "$TMPDIR_FETCH/blanks.zip"; then
        for sz in 400 800; do
            [ -f "gryphel-mfs${sz}k.image" ] && continue
            unzip -qo "$TMPDIR_FETCH/blanks.zip" "blanks-1.1/dc42/mfs${sz}K.zip" -d "$TMPDIR_FETCH" \
                && unzip -qo "$TMPDIR_FETCH/blanks-1.1/dc42/mfs${sz}K.zip" -d "$TMPDIR_FETCH/inner" \
                && cp "$TMPDIR_FETCH/inner/mfs${sz}K.dsk" "gryphel-mfs${sz}k.image" \
                && echo "got   gryphel-mfs${sz}k.image" \
                || echo "FAIL  gryphel-mfs${sz}k.image (zip layout changed?)"
        done
    else
        echo "FAIL  gryphel blanks (download)"
    fi
fi

# 3. Finder 1.0 system disk (earlymacintosh.org) — plain zip, no StuffIt needed.
if ! have "Finder 1.0.image"; then
    if curl -fsSL "https://www.earlymacintosh.org/disk_images/Finder%201.0.zip" \
            -o "$TMPDIR_FETCH/finder10.zip"; then
        unzip -qo "$TMPDIR_FETCH/finder10.zip" -d "$TMPDIR_FETCH/finder10"
        img="$(find "$TMPDIR_FETCH/finder10" -type f \( -name '*.image' -o -name '*.img' -o -name '*.dsk' \) | head -1)"
        if [ -n "$img" ]; then
            cp "$img" "Finder 1.0.image" && echo "got   Finder 1.0.image"
        else
            echo "FAIL  Finder 1.0.image (no image inside zip)"
        fi
    else
        echo "FAIL  Finder 1.0.image (download)"
    fi
fi

# 4. System 1.1 / 2.0 disks (earlymacintosh.org) — StuffIt archives; need unar.
if command -v unar >/dev/null 2>&1; then
    for name in "1.1 System Disk" "2.0 System Disk"; do
        dest="$name.image"
        have "$dest" && continue
        url="https://www.earlymacintosh.org/disk_images/$(printf '%s' "$name" | sed 's/ /%20/g').sit"
        # These downloads are a zip containing a StuffIt archive containing
        # the DiskCopy 4.2 image, so unar runs twice.
        if curl -fsSL "$url" -o "$TMPDIR_FETCH/dl.sit" \
           && unar -quiet -force-overwrite -output-directory "$TMPDIR_FETCH/sit" "$TMPDIR_FETCH/dl.sit"; then
            inner="$(find "$TMPDIR_FETCH/sit" -type f -name '*.sit' | head -1)"
            [ -n "$inner" ] && unar -quiet -force-overwrite \
                -output-directory "$TMPDIR_FETCH/sit2" "$inner"
            img="$(find "$TMPDIR_FETCH/sit2" "$TMPDIR_FETCH/sit" -type f -name '*.image' 2>/dev/null | head -1)"
            if [ -n "$img" ]; then
                cp "$img" "$dest" && echo "got   $dest"
            else
                echo "FAIL  $dest (no .image inside archive)"
            fi
            rm -rf "$TMPDIR_FETCH/sit" "$TMPDIR_FETCH/sit2" "$TMPDIR_FETCH/dl.sit"
        else
            echo "FAIL  $dest ($url)"
        fi
    done
else
    echo "note: 'unar' not found — skipping StuffIt-wrapped System 1.1/2.0 disks"
    echo "      install with: brew install unar   (tests skip those images gracefully)"
fi

echo
echo "tests/images now contains:"
ls -l .
