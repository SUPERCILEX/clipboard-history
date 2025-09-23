use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    mem::MaybeUninit,
};

use ringboard_core::{
    IoErr, RingAndIndex, polyfills::BorrowedBuf, protocol::IdNotFoundError, read_at_to_end,
};
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
                        let mut buf = [MaybeUninit::uninit(); 4096];
                        let mut buf = BorrowedBuf::from(buf.as_mut_slice());
                        read_at_to_end(&*file, buf.unfilled(), 0)
                            .map_io_err(|| format!("Failed to read file: {file:?}"))?;
                        buf.filled().hash(&mut data_hasher);
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
