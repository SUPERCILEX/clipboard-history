use std::{
    fs::File,
    hash::{Hash, Hasher},
};

use log::info;
use memmap2::Mmap;
use ringboard_core::{
    dirs::data_dir,
    protocol::{composite_id, decompose_id, RingKind},
};
use ringboard_sdk::{DatabaseReader, EntryReader, Kind, RingReader};
use rustc_hash::FxHasher;
use rustix::fs::{statx, AtFlags, StatxFlags};

pub struct CopyDeduplication {
    main: ArrayMap<2048>,
    favorites: ArrayMap<16>,

    database: DatabaseReader,
    reader: EntryReader,
}

#[derive(Copy, Clone, Debug)]
pub enum CopyData<'a> {
    Slice(&'a [u8]),
    File(&'a File),
}

impl CopyDeduplication {
    pub fn new() -> Result<Self, ringboard_core::Error> {
        let mut main = ArrayMap::default();
        let mut favorites = ArrayMap::default();
        let (database, mut reader) = {
            let mut database = data_dir();
            (
                DatabaseReader::open(&mut database)?,
                EntryReader::open(&mut database)?,
            )
        };

        {
            let fav_history = favorites.ids.len();
            let main_history = main.ids.len();

            let mut load = |mut iter: RingReader, count| {
                let count = u32::try_from(count).unwrap().min(iter.ring().len());
                let tail = iter.ring().write_head();
                info!(
                    "Loading the previous {count} entries in {:?} ring for duplicate detection.",
                    iter.kind()
                );

                iter.reset_to(
                    tail,
                    if count <= tail {
                        tail - count
                    } else {
                        iter.ring().len() - (count - tail)
                    },
                );

                for entry in iter {
                    if let Some(hash) = match entry.kind() {
                        Kind::Bucket(_) => entry.to_slice_growable(&mut reader).ok().map(|data| {
                            Self::hash(CopyData::Slice(&data), u64::try_from(data.len()).unwrap())
                        }),
                        Kind::File => entry.to_file_growable(&mut reader).ok().and_then(|file| {
                            Some(Self::hash(
                                CopyData::File(&file),
                                statx(&*file, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                                    .ok()?
                                    .stx_size,
                            ))
                        }),
                    } {
                        Self::remember_(&mut main, &mut favorites, hash, entry.id());
                    }
                }
            };

            load(database.favorites(), fav_history);
            load(database.main(), main_history);
        }

        Ok(Self {
            main,
            favorites,
            database,
            reader,
        })
    }

    pub fn hash(data: CopyData, len: u64) -> u64 {
        let mut data_hasher = FxHasher::default();
        if len > 4096 {
            len.hash(&mut data_hasher);
        } else {
            match data {
                CopyData::Slice(s) => s.hash(&mut data_hasher),
                CopyData::File(f) => {
                    if let Ok(m) = unsafe { Mmap::map(f) } {
                        m.hash(&mut data_hasher);
                    }
                }
            }
        }
        data_hasher.finish()
    }

    pub fn check(&mut self, hash: u64, data: CopyData) -> Option<u64> {
        self.favorites
            .get(hash)
            .map(|id| (id, RingKind::Favorites))
            .or_else(|| self.main.get(hash).map(|id| (id, RingKind::Main)))
            .map(|(id, ring)| composite_id(ring, id))
            .and_then(|id| {
                let entry = unsafe { self.database.growable_get(id).ok()? };
                match data {
                    CopyData::Slice(data) => {
                        *entry.to_slice_growable(&mut self.reader).ok()? == data
                    }
                    CopyData::File(data) => {
                        let a =
                            unsafe { Mmap::map(&*entry.to_file_growable(&mut self.reader).ok()?) }
                                .ok()?;
                        let b = unsafe { Mmap::map(data) }.ok()?;

                        *a == *b
                    }
                }
                .then_some(id)
            })
    }

    pub fn remember(&mut self, hash: u64, id: u64) {
        Self::remember_(&mut self.main, &mut self.favorites, hash, id);
    }

    fn remember_<const A: usize, const B: usize>(
        main: &mut ArrayMap<A>,
        favorites: &mut ArrayMap<B>,
        hash: u64,
        id: u64,
    ) {
        let Ok((ring, id)) = decompose_id(id) else {
            return;
        };

        match ring {
            RingKind::Favorites => favorites.remember(hash, id),
            RingKind::Main => main.remember(hash, id),
        }
    }
}

struct ArrayMap<const SLOTS: usize> {
    ids: [u32; SLOTS],
}

impl<const N: usize> Default for ArrayMap<N> {
    fn default() -> Self {
        Self { ids: [0; N] }
    }
}

impl<const N: usize> ArrayMap<N> {
    fn hash_to_index(mut hash: u64) -> usize {
        const { assert!(N.is_power_of_two()) };

        let mut result = 0;
        while hash > 0 {
            result ^= hash & u64::try_from(N - 1).unwrap();
            hash >>= const { N.ilog2() };
        }
        usize::try_from(result).unwrap()
    }

    pub fn get(&self, hash: u64) -> Option<u32> {
        let id = self.ids[Self::hash_to_index(hash)];
        if id == 0 { None } else { Some(id - 1) }
    }

    pub fn remember(&mut self, hash: u64, id: u32) {
        self.ids[Self::hash_to_index(hash)] = id + 1;
    }
}
