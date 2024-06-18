use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
};

use ringboard_core::{protocol::RingKind, ring::MAX_ENTRIES};
use rustc_hash::FxHasher;
use smallvec::SmallVec;

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct RingAndIndex(u32);

impl RingAndIndex {
    fn new(ring: RingKind, index: u32) -> Self {
        const {
            assert!(size_of::<RingKind>() == size_of::<u8>());
        }
        debug_assert!(index <= MAX_ENTRIES);

        Self((index << u8::BITS) | (ring as u32))
    }
}

#[derive(Default)]
pub struct DuplicateDetector {
    hashes: BTreeMap<u32, SmallVec<[RingAndIndex; 1]>>,
}

enum Data<'a> {
    Lenth(u64),
    Bytes(&'a [u8]),
}

impl DuplicateDetector {
    pub fn add_len_entry(&mut self, ring: RingKind, index: u32, len: u64) -> bool {
        self.add_entry(RingAndIndex::new(ring, index), Data::Lenth(len))
    }

    pub fn add_data_entry(&mut self, ring: RingKind, index: u32, bytes: &[u8]) -> bool {
        self.add_entry(RingAndIndex::new(ring, index), Data::Bytes(bytes))
    }

    fn add_entry(&mut self, entry: RingAndIndex, data: Data) -> bool {
        let hash = {
            let mut data_hasher = FxHasher::default();
            match data {
                Data::Lenth(len) => len.hash(&mut data_hasher),
                Data::Bytes(bytes) => bytes.hash(&mut data_hasher),
            }
            data_hasher.finish()
        };

        let entries = self.hashes.entry(hash as u32).or_default();
        let duplicate = entries.contains(&entry);
        entries.push(entry);
        duplicate
    }
}
