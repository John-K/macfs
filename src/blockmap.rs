//! The Allocation Block Map (ABM): 12-bit entries, two packed into every three
//! bytes, stored immediately after the 64-byte MDB.
//!
//! Allocation block numbering starts at 2, so the entry for block `N` lives at
//! index `i = N - 2`. Within the packed bytes, entry `i` starts at byte offset
//! `o = (i / 2) * 3`:
//!
//! * even `i`: `(b[o] << 4) | (b[o + 1] >> 4)`
//! * odd `i`:  `((b[o + 1] & 0x0F) << 8) | b[o + 2]`
//!
//! Entry values: `0x000` free, `0x001` last block of a file, `0x002..=0xFEF`
//! the next block in the file's chain, `0xFFF` reserved (directory / system).

use crate::error::{MfsError, Result};

/// The block is not part of any file.
pub(crate) const FREE: u16 = 0x000;
/// The block is the last one in its file's chain.
pub(crate) const LAST: u16 = 0x001;
/// The block is reserved (directory or system use) and never part of a chain.
pub(crate) const RESERVED: u16 = 0xFFF;

/// The first valid allocation block number.
pub(crate) const FIRST_BLOCK: u16 = 2;

/// An unpacked allocation block map. `entries[0]` describes block 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockMap {
    entries: Vec<u16>,
}

/// Number of packed bytes required to hold `n_blocks` 12-bit entries.
///
/// For an odd count the final entry only occupies two of its triplet's three
/// bytes, so this rounds up on nibbles rather than on whole triplets.
pub(crate) fn packed_len(n_blocks: u16) -> usize {
    (n_blocks as usize * 3).div_ceil(2)
}

impl BlockMap {
    /// Decode `n_blocks` 12-bit entries out of `raw`.
    ///
    /// `raw` may be longer than needed (the map is padded out to a sector
    /// boundary on disk); trailing bytes are ignored.
    pub(crate) fn unpack(raw: &[u8], n_blocks: u16) -> Result<BlockMap> {
        let needed = packed_len(n_blocks);
        if raw.len() < needed {
            return Err(MfsError::CorruptVolume(format!(
                "allocation block map is {} bytes, need {needed} for {n_blocks} blocks",
                raw.len()
            )));
        }
        let mut entries = Vec::with_capacity(n_blocks as usize);
        for i in 0..n_blocks as usize {
            let o = (i / 2) * 3;
            let v = if i % 2 == 0 {
                ((raw[o] as u16) << 4) | ((raw[o + 1] >> 4) as u16)
            } else {
                (((raw[o + 1] & 0x0F) as u16) << 8) | (raw[o + 2] as u16)
            };
            entries.push(v);
        }
        Ok(BlockMap { entries })
    }

    /// Encode the map into `out` — the exact inverse of [`BlockMap::unpack`].
    ///
    /// Only the nibbles this map owns are touched (read-modify-write), so an
    /// odd final entry never clobbers the low nibble of its last byte and
    /// vice versa. `out` is expected to have been zeroed by the caller.
    ///
    /// # Panics
    /// Panics if `out` is shorter than [`packed_len`] of this map.
    pub(crate) fn pack(&self, out: &mut [u8]) {
        let needed = packed_len(self.len());
        assert!(
            out.len() >= needed,
            "allocation block map output is {} bytes, need {needed}",
            out.len()
        );
        for (i, &v) in self.entries.iter().enumerate() {
            let v = v & 0x0FFF;
            let o = (i / 2) * 3;
            if i % 2 == 0 {
                out[o] = (v >> 4) as u8;
                out[o + 1] = (out[o + 1] & 0x0F) | (((v & 0x000F) as u8) << 4);
            } else {
                out[o + 1] = (out[o + 1] & 0xF0) | ((v >> 8) as u8 & 0x0F);
                out[o + 2] = (v & 0x00FF) as u8;
            }
        }
    }

    /// Build a map of `n_blocks` entries, all free.
    pub(crate) fn new_empty(n_blocks: u16) -> BlockMap {
        BlockMap { entries: vec![FREE; n_blocks as usize] }
    }

    /// Number of allocation blocks described by this map.
    pub(crate) fn len(&self) -> u16 {
        self.entries.len() as u16
    }

    /// Whether `block` is a valid allocation block number for this map.
    pub(crate) fn in_range(&self, block: u16) -> bool {
        block >= FIRST_BLOCK && (block as usize) < FIRST_BLOCK as usize + self.entries.len()
    }

    /// The map entry for allocation block `block`.
    ///
    /// # Panics
    /// Panics if `block` is out of range — callers inside the crate validate
    /// block numbers against the MDB before getting here.
    pub(crate) fn get(&self, block: u16) -> u16 {
        assert!(self.in_range(block), "allocation block {block} out of range");
        self.entries[block as usize - FIRST_BLOCK as usize]
    }

    /// Set the map entry for allocation block `block`.
    ///
    /// # Panics
    /// Panics if `block` is out of range.
    pub(crate) fn set(&mut self, block: u16, v: u16) {
        assert!(self.in_range(block), "allocation block {block} out of range");
        self.entries[block as usize - FIRST_BLOCK as usize] = v & 0x0FFF;
    }

    /// Number of free allocation blocks.
    pub(crate) fn free_count(&self) -> u32 {
        self.entries.iter().filter(|&&v| v == FREE).count() as u32
    }

    /// Follow the chain of blocks starting at `start`, in order, up to and
    /// including the block whose entry is [`LAST`].
    ///
    /// Fails with `CorruptVolume` on an out-of-range block number, a free or
    /// reserved entry inside the chain, or a chain longer than the map (a
    /// cycle).
    pub(crate) fn chain(&self, start: u16) -> Result<Vec<u16>> {
        let limit = self.entries.len();
        let mut out: Vec<u16> = Vec::new();
        let mut cur = start;
        loop {
            if !self.in_range(cur) {
                return Err(MfsError::CorruptVolume(format!(
                    "block chain references block {cur}, outside 2..{}",
                    FIRST_BLOCK as usize + limit
                )));
            }
            let v = self.get(cur);
            if v == FREE {
                return Err(MfsError::CorruptVolume(format!(
                    "block chain enters free block {cur}"
                )));
            }
            if v == RESERVED {
                return Err(MfsError::CorruptVolume(format!(
                    "block chain enters reserved block {cur}"
                )));
            }
            out.push(cur);
            if out.len() > limit {
                return Err(MfsError::CorruptVolume(format!(
                    "block chain starting at {start} exceeds {limit} blocks (cycle)"
                )));
            }
            if v == LAST {
                return Ok(out);
            }
            cur = v;
        }
    }

    /// First-fit allocation of `n` blocks, linked into a single chain.
    ///
    /// On success the returned blocks are in chain order: each one's map entry
    /// points at the next, and the final entry is [`LAST`]. If fewer than `n`
    /// blocks are free the map is left completely untouched and
    /// `VolumeFull` is returned.
    pub(crate) fn allocate(&mut self, n: u32) -> Result<Vec<u16>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let free = self.free_count();
        if free < n {
            return Err(MfsError::VolumeFull { needed_blocks: n, free_blocks: free });
        }

        let mut blocks: Vec<u16> = Vec::with_capacity(n as usize);
        for (i, &v) in self.entries.iter().enumerate() {
            if v == FREE {
                blocks.push(i as u16 + FIRST_BLOCK);
                if blocks.len() as u32 == n {
                    break;
                }
            }
        }
        debug_assert_eq!(blocks.len() as u32, n);

        for i in 0..blocks.len() {
            let next = if i + 1 < blocks.len() { blocks[i + 1] } else { LAST };
            self.set(blocks[i], next);
        }
        Ok(blocks)
    }

    /// Free every block in the chain starting at `start`.
    ///
    /// Validates the whole chain before mutating, so a corrupt chain leaves the
    /// map untouched.
    pub(crate) fn free_chain(&mut self, start: u16) -> Result<()> {
        let blocks = self.chain(start)?;
        for b in blocks {
            self.set(b, FREE);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny xorshift32 so the tests pull in no extra dependencies.
    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
    }

    fn random_entries(seed: u32, n: u16) -> Vec<u16> {
        let mut rng = Rng(seed);
        (0..n).map(|_| (rng.next() & 0x0FFF) as u16).collect()
    }

    fn map_of(entries: &[u16]) -> BlockMap {
        let mut m = BlockMap::new_empty(entries.len() as u16);
        for (i, &v) in entries.iter().enumerate() {
            m.set(i as u16 + FIRST_BLOCK, v);
        }
        m
    }

    #[test]
    fn hand_computed_vector() {
        let raw = [0xABu8, 0xCD, 0xEF];
        let m = BlockMap::unpack(&raw, 2).unwrap();
        assert_eq!(m.get(2), 0xABC);
        assert_eq!(m.get(3), 0xDEF);

        let mut out = [0u8; 3];
        m.pack(&mut out);
        assert_eq!(out, raw);

        // A single entry only claims the first byte and the high nibble of the second.
        let m1 = BlockMap::unpack(&raw, 1).unwrap();
        assert_eq!(m1.get(2), 0xABC);
        assert_eq!(packed_len(1), 2);
    }

    #[test]
    fn packed_len_rounds_on_nibbles() {
        assert_eq!(packed_len(0), 0);
        assert_eq!(packed_len(1), 2);
        assert_eq!(packed_len(2), 3);
        assert_eq!(packed_len(3), 5);
        assert_eq!(packed_len(4), 6);
        assert_eq!(packed_len(391), 587);
    }

    #[test]
    fn entries_to_bytes_and_back_round_trips() {
        for n in [0u16, 1, 2, 3, 4, 5, 17, 100, 391, 0xFED] {
            let entries = random_entries(0x1234_5678 ^ n as u32, n);
            let m = map_of(&entries);
            let mut buf = vec![0u8; packed_len(n)];
            m.pack(&mut buf);
            let back = BlockMap::unpack(&buf, n).unwrap();
            assert_eq!(back, m, "entries round-trip failed for n = {n}");
            for (i, &v) in entries.iter().enumerate() {
                assert_eq!(back.get(i as u16 + FIRST_BLOCK), v);
            }
        }
    }

    #[test]
    fn bytes_to_entries_and_back_round_trips() {
        for n in [1u16, 2, 3, 4, 5, 17, 100, 391] {
            let mut rng = Rng(0x9E37_79B9 ^ n as u32);
            let need = packed_len(n);
            let mut raw: Vec<u8> = (0..need).map(|_| rng.next() as u8).collect();
            // For an odd entry count the low nibble of the final byte belongs to
            // nobody, so it is not expected to survive a repack.
            if n % 2 == 1 {
                let last = need - 1;
                raw[last] &= 0xF0;
            }
            let m = BlockMap::unpack(&raw, n).unwrap();
            let mut out = vec![0u8; need];
            m.pack(&mut out);
            assert_eq!(out, raw, "byte round-trip failed for n = {n}");
        }
    }

    #[test]
    fn pack_does_not_clobber_unowned_nibbles() {
        // Odd count: the last byte's low nibble must be left as the caller had it.
        let m = map_of(&[0x111, 0x222, 0x333]);
        let mut out = vec![0xFFu8; packed_len(3)];
        m.pack(&mut out);
        assert_eq!(out[3], 0x33);
        assert_eq!(out[4], 0x3F, "low nibble of the final byte was clobbered");
    }

    #[test]
    fn unpack_ignores_trailing_padding() {
        let mut raw = vec![0u8; 512];
        raw[0..3].copy_from_slice(&[0xAB, 0xCD, 0xEF]);
        let m = BlockMap::unpack(&raw, 2).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(3), 0xDEF);
    }

    #[test]
    fn unpack_rejects_short_input() {
        let raw = [0u8; 4];
        assert!(matches!(
            BlockMap::unpack(&raw, 4), // needs 6
            Err(MfsError::CorruptVolume(_))
        ));
        // Exactly enough is fine, including the odd-count short final triplet.
        assert!(BlockMap::unpack(&raw[..2], 1).is_ok());
        assert!(BlockMap::unpack(&raw[..3], 2).is_ok());
    }

    #[test]
    fn chain_follows_hand_built_chain() {
        // blocks 2 -> 5 -> 3 -> end; block 4 free; block 6 reserved
        let m = map_of(&[5, LAST, FREE, 3, RESERVED]); // entries for blocks 2,3,4,5,6
        assert_eq!(m.chain(2).unwrap(), vec![2, 5, 3]);
        // A one-block file.
        assert_eq!(m.chain(3).unwrap(), vec![3]);
    }

    #[test]
    fn chain_rejects_bad_starts_and_bad_entries() {
        let m = map_of(&[5, LAST, FREE, 3, RESERVED]); // blocks 2..=6
        // Out of range on both ends.
        assert!(matches!(m.chain(0), Err(MfsError::CorruptVolume(_))));
        assert!(matches!(m.chain(1), Err(MfsError::CorruptVolume(_))));
        assert!(matches!(m.chain(7), Err(MfsError::CorruptVolume(_))));
        // Free block.
        assert!(matches!(m.chain(4), Err(MfsError::CorruptVolume(_))));
        // Reserved block.
        assert!(matches!(m.chain(6), Err(MfsError::CorruptVolume(_))));
    }

    #[test]
    fn chain_detects_cycles() {
        // 2 -> 3 -> 2 -> ... forever
        let m = map_of(&[3, 2]);
        assert!(matches!(m.chain(2), Err(MfsError::CorruptVolume(_))));
        // Self-loop.
        let m = map_of(&[2, 1]);
        assert!(matches!(m.chain(2), Err(MfsError::CorruptVolume(_))));
        // A chain that walks off the end of the map.
        let m = map_of(&[9, 1]);
        assert!(matches!(m.chain(2), Err(MfsError::CorruptVolume(_))));
    }

    #[test]
    fn allocate_links_a_chain_and_free_restores_the_count() {
        let mut m = BlockMap::new_empty(10);
        assert_eq!(m.free_count(), 10);

        let a = m.allocate(3).unwrap();
        assert_eq!(a, vec![2, 3, 4]);
        assert_eq!(m.get(2), 3);
        assert_eq!(m.get(3), 4);
        assert_eq!(m.get(4), LAST);
        assert_eq!(m.free_count(), 7);
        assert_eq!(m.chain(2).unwrap(), a);

        let b = m.allocate(2).unwrap();
        assert_eq!(b, vec![5, 6]);
        assert_eq!(m.free_count(), 5);

        // First-fit fills the hole left by a freed chain.
        m.free_chain(2).unwrap();
        assert_eq!(m.free_count(), 8);
        let c = m.allocate(4).unwrap();
        assert_eq!(c, vec![2, 3, 4, 7]);
        assert_eq!(m.chain(2).unwrap(), c);
        assert_eq!(m.free_count(), 4);

        // Everything freed again restores the original map exactly.
        m.free_chain(2).unwrap();
        m.free_chain(5).unwrap();
        assert_eq!(m.free_count(), 10);
        assert_eq!(m, BlockMap::new_empty(10));
    }

    #[test]
    fn allocate_zero_is_a_no_op() {
        let mut m = BlockMap::new_empty(4);
        assert_eq!(m.allocate(0).unwrap(), Vec::<u16>::new());
        assert_eq!(m, BlockMap::new_empty(4));
    }

    #[test]
    fn allocate_can_take_the_whole_map() {
        let mut m = BlockMap::new_empty(4);
        assert_eq!(m.allocate(4).unwrap(), vec![2, 3, 4, 5]);
        assert_eq!(m.free_count(), 0);
        assert_eq!(m.chain(2).unwrap(), vec![2, 3, 4, 5]);
    }

    #[test]
    fn allocate_insufficient_reports_volume_full_and_changes_nothing() {
        let mut m = BlockMap::new_empty(5);
        m.allocate(3).unwrap(); // 2 free left
        let before = m.clone();
        match m.allocate(3) {
            Err(MfsError::VolumeFull { needed_blocks, free_blocks }) => {
                assert_eq!(needed_blocks, 3);
                assert_eq!(free_blocks, 2);
            }
            other => panic!("expected VolumeFull, got {other:?}"),
        }
        assert_eq!(m, before, "a failed allocation must not modify the map");
    }

    #[test]
    fn free_chain_rejects_a_corrupt_chain_without_mutating() {
        let mut m = map_of(&[3, 2]); // cycle
        let before = m.clone();
        assert!(m.free_chain(2).is_err());
        assert_eq!(m, before);
    }
}
