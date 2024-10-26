use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
};

use ringboard_core::{IoErr, RingAndIndex, protocol::IdNotFoundError, ring::Mmap};
use rustc_hash::FxHasher;
use rustix::fs::{AtFlags, StatxFlags, statx};
use smallvec::SmallVec;

use crate::{DatabaseReader, Entry, EntryReader, Kind};

#[derive(Default)]
pub struct DuplicateDetector {
    hashes: BTreeMap<u32, SmallVec<RingAndIndex, 4>>,
}

const _: () = assert!(size_of::<SmallVec<RingAndIndex, 4>>() <= size_of::<Vec<RingAndIndex>>());

impl DuplicateDetector {
    pub fn add_entry(
        &mut self,
        entry: &Entry,
        database: &DatabaseReader,
        reader: &mut EntryReader,
    ) -> Result<bool, ringboard_core::Error> {
        let hash = {
            let mut data_hasher = FxHasher::default();
            match entry.kind() {
                Kind::Bucket(_) => entry.to_slice(reader)?.hash(&mut data_hasher),
                Kind::File => {
                    let file = entry.to_file(reader)?;
                    let len = statx(&*file, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                        .map_io_err(|| format!("Failed to statx file: {file:?}"))?
                        .stx_size;

                    if len >= 4096 {
                        len.hash(&mut data_hasher);
                    } else {
                        Mmap::from(&*file)
                            .map_io_err(|| format!("Failed to mmap file: {file:?}"))?
                            .hash(&mut data_hasher);
                    }
                }
            }
            data_hasher.finish()
        };

        let entries = self
            .hashes
            .entry(u32::try_from(hash & u64::from(u32::MAX)).unwrap())
            .or_default();
        if !entries.is_empty() {
            let data = entry.to_slice_raw(reader)?.unwrap();
            for &entry in &*entries {
                let entry = database.get_raw(entry.id())?;
                if **data
                    == **entry
                        .to_slice_raw(reader)?
                        .ok_or_else(|| IdNotFoundError::Entry(entry.index()))?
                {
                    return Ok(true);
                }
            }
        }
        entries.push(RingAndIndex::new(entry.ring(), entry.index()));
        Ok(false)
    }
}
