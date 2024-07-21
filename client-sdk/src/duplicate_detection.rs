use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
};

use ringboard_core::{ring::Mmap, IoErr, RingAndIndex};
use rustc_hash::FxHasher;
use rustix::fs::{statx, AtFlags, StatxFlags};
use smallvec::SmallVec;

use crate::{DatabaseReader, Entry, EntryReader, Kind};

#[derive(Default)]
pub struct DuplicateDetector {
    hashes: BTreeMap<u32, SmallVec<[RingAndIndex; 1]>>,
}

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
                        Mmap::from(&*entry.to_file(reader)?)
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
        let mut duplicate = false;
        if !entries.is_empty() {
            let data = entry.to_file(reader)?;
            let data =
                Mmap::from(&*data).map_io_err(|| format!("Failed to mmap file: {data:?}"))?;
            for &entry in &*entries {
                let entry = database.get_raw(entry.id())?;
                if match entry.kind() {
                    Kind::Bucket(_) => *data == **entry.to_slice(reader)?,
                    Kind::File => {
                        let file = entry.to_file(reader)?;
                        *data
                            == *Mmap::from(&*file)
                                .map_io_err(|| format!("Failed to mmap file: {file:?}"))?
                    }
                } {
                    duplicate = true;
                    break;
                }
            }
        }
        entries.push(RingAndIndex::new(entry.ring(), entry.index()));
        Ok(duplicate)
    }
}
