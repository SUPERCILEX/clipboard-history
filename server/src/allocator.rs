use std::{
    array,
    cmp::{min, Reverse},
    collections::BinaryHeap,
    fmt::Debug,
    fs,
    fs::File,
    io,
    io::{ErrorKind, IoSlice, Read, Write},
    mem::ManuallyDrop,
    ops::{Index, IndexMut},
    os::{fd::OwnedFd, unix::fs::FileExt},
    path::PathBuf,
    slice,
};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use bitvec::{order::Lsb0, vec::BitVec};
use log::{debug, error, info, trace, warn};
use ringboard_core::{
    bucket_to_length, copy_file_range_all, direct_file_name, open_buckets,
    protocol::{
        composite_id, decompose_id, AddResponse, GarbageCollectResponse, IdNotFoundError, MimeType,
        MoveToFrontResponse, RemoveResponse, RingKind, SwapResponse,
    },
    ring,
    ring::{entries_to_offset, Entry, Header, InitializedEntry, RawEntry, Ring},
    size_to_bucket, IoErr, PathView, RingAndIndex, NUM_BUCKETS, TEXT_MIMES,
};
use rustix::{
    fs::{
        fsetxattr, ftruncate, openat, renameat, renameat_with, unlinkat, AtFlags, Mode, OFlags,
        RenameFlags, XattrFlags, CWD,
    },
    path::Arg,
};

use crate::{utils::link_tmp_file, CliError};

#[derive(Debug)]
struct RingWriter {
    ring: File,
}

impl RingWriter {
    fn open<P: Arg + Copy + Debug>(path: P) -> Result<Self, CliError> {
        let ring = match openat(CWD, path, OFlags::WRONLY, Mode::empty()) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let fd = openat(
                    CWD,
                    path,
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_io_err(|| format!("Failed to create Ringboard database: {path:?}"))?;
                let mut f = File::from(fd);

                {
                    let Header {
                        magic,
                        version,
                        write_head,
                    } = &Header::default();
                    f.write_all_vectored(&mut [
                        IoSlice::new(magic),
                        IoSlice::new(slice::from_ref(version)),
                        IoSlice::new(&write_head.to_le_bytes()),
                    ])
                    .map_io_err(|| {
                        format!("Failed to write header to Ringboard database: {path:?}")
                    })?;
                }

                f
            }
            r => File::from(r.map_io_err(|| {
                format!("Failed to open Ringboard database for writing: {path:?}")
            })?),
        };

        Ok(Self { ring })
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn write(&mut self, entry: Entry, at: u32) -> ringboard_core::Result<()> {
        debug!("Writing entry to position {at}: {entry:?}");
        self.ring
            .write_all_at(&RawEntry::from(entry).to_le_bytes(), entries_to_offset(at))
            .map_io_err(|| format!("Failed to write entry to Ringboard database: {entry:?}"))
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn set_write_head(&mut self, head: u32) -> ringboard_core::Result<()> {
        debug!("Setting write head to {head}.");
        self.ring
            .write_all_at(
                &head.to_le_bytes(),
                u64::try_from(ring::MAGIC.len() + size_of_val(&ring::VERSION)).unwrap(),
            )
            .map_io_err(|| format!("Failed to update Ringboard write head: {head}"))
    }
}

#[derive(Debug)]
struct WritableRing {
    writer: RingWriter,
    ring: Ring,
}

#[derive(Debug)]
struct Rings([WritableRing; 2]);

impl Index<RingKind> for Rings {
    type Output = WritableRing;

    fn index(&self, index: RingKind) -> &Self::Output {
        &self.0[index as usize]
    }
}

impl IndexMut<RingKind> for Rings {
    fn index_mut(&mut self, index: RingKind) -> &mut Self::Output {
        &mut self.0[index as usize]
    }
}

#[derive(Debug)]
pub struct Allocator {
    rings: Rings,
    data: AllocatorData,
}

#[derive(Debug)]
struct AllocatorData {
    buckets: Buckets,
    direct_dir: OwnedFd,
}

#[derive(Debug)]
struct Buckets {
    files: [File; NUM_BUCKETS],
    slot_counts: [u32; NUM_BUCKETS],
    free_lists: FreeLists,
}

#[derive(Debug)]
struct FreeLists {
    lists: RawFreeLists,
    file: File,
}

#[derive(Encode, Decode, Default, Debug)]
struct RawFreeLists([Vec<u32>; NUM_BUCKETS]);

struct BucketSlotGuard<'a> {
    id: u32,
    free_list: &'a mut Vec<u32>,
}

impl BucketSlotGuard<'_> {
    fn into_inner(self) -> u32 {
        let this = ManuallyDrop::new(self);
        this.id
    }
}

impl Drop for BucketSlotGuard<'_> {
    fn drop(&mut self) {
        self.free_list.push(self.id);
    }
}

impl FreeLists {
    fn load(data_dir: &mut PathBuf) -> Result<Self, CliError> {
        let path = PathView::new(data_dir, "free-lists");
        let mut file = match openat(CWD, &*path, OFlags::RDWR, Mode::empty()) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let file = File::from(
                    openat(
                        CWD,
                        &*path,
                        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
                        Mode::RUSR | Mode::WUSR,
                    )
                    .map_io_err(|| format!("Failed to create free lists file: {path:?}"))?,
                );
                return Ok(Self {
                    lists: RawFreeLists::default(),
                    file,
                });
            }
            r => File::from(r.map_io_err(|| format!("Failed to open free lists: {path:?}"))?),
        };

        {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_io_err(|| format!("Failed to read free lists: {path:?}"))?;

            if !bytes.is_empty() {
                file.set_len(0)
                    .map_io_err(|| format!("Failed to truncate free lists: {path:?}"))?;
                match bitcode::decode(&bytes) {
                    Ok(lists) => return Ok(Self { lists, file }),
                    Err(e) => {
                        error!("Corrupted free lists file: {path:?}\nError: {e:?}");
                    }
                }
            }
        }
        drop(path);
        warn!("Reconstructing allocator free lists.");

        let mut allocations = [BitVec::<usize, Lsb0>::EMPTY; NUM_BUCKETS];
        for ring in [RingKind::Favorites, RingKind::Main] {
            let ring = Ring::open(0, &*PathView::new(data_dir, ring.file_name()))?;
            for entry in (0..ring.len()).filter_map(|i| ring.get(i)) {
                match entry {
                    Entry::Bucketed(entry) => {
                        let slots = &mut allocations[usize::from(size_to_bucket(entry.size()))];
                        let index = usize::try_from(entry.index()).unwrap();
                        if slots.len() <= index {
                            slots.resize(index, false);
                            slots.push(true);
                        } else {
                            slots.set(index, true);
                        }
                    }
                    Entry::Uninitialized | Entry::File => continue,
                }
            }
        }

        Ok(Self {
            lists: RawFreeLists(allocations.map(|slots| {
                slots
                    .iter_zeros()
                    .map(|i| u32::try_from(i).unwrap())
                    .collect()
            })),
            file,
        })
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn save(&mut self) -> Result<(), CliError> {
        info!("Saving allocator free list to disk.");
        let bytes = bitcode::encode(&self.lists);
        self.file
            .write_all_at(&bytes, 0)
            .map_io_err(|| "Failed to write free lists.")?;
        Ok(())
    }

    fn alloc(&mut self, bucket: usize) -> Option<BucketSlotGuard> {
        let free_list = &mut self.lists.0[bucket];
        free_list.pop().map(|id| BucketSlotGuard { id, free_list })
    }

    fn free(&mut self, bucket: usize, index: u32) {
        debug!("Freeing slot {index} for bucket {bucket}.");
        self.lists.0[bucket].push(index);
    }
}

impl Allocator {
    pub fn open(mut data_dir: PathBuf) -> Result<Self, CliError> {
        let mut open_ring = |kind: RingKind| -> Result<_, CliError> {
            let ring = PathView::new(&mut data_dir, kind.file_name());
            Ok(WritableRing {
                writer: RingWriter::open(&*ring)?,
                ring: Ring::open(kind.default_max_entries(), &*ring)?,
            })
        };
        let main_ring = open_ring(RingKind::Main)?;
        let favorites_ring = open_ring(RingKind::Favorites)?;

        let mut create_dir = |name| {
            let dir = PathView::new(&mut data_dir, name);
            fs::create_dir_all(&dir).map_io_err(|| format!("Failed to create directory: {dir:?}"))
        };
        create_dir("direct")?;
        create_dir("buckets")?;

        let (buckets, slot_counts) = {
            let mut buckets = PathView::new(&mut data_dir, "buckets");
            open_buckets(|name| {
                let file = PathView::new(&mut buckets, name);
                openat(
                    CWD,
                    &*file,
                    OFlags::RDWR | OFlags::CREATE,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_io_err(|| format!("Failed to create bucket: {file:?}"))
            })?
        };
        let slot_counts = {
            let mut i = 0;
            slot_counts.map(|len| {
                let slots = u32::try_from(len >> (i + 2)).unwrap();
                i += 1;
                slots
            })
        };

        let direct_dir = {
            let file = PathView::new(&mut data_dir, "direct");
            openat(CWD, &*file, OFlags::DIRECTORY | OFlags::PATH, Mode::empty())
                .map_io_err(|| format!("Failed to open directory: {file:?}"))
        }?;

        let free_lists = FreeLists::load(&mut data_dir)?;

        Ok(Self {
            rings: Rings([favorites_ring, main_ring]),
            data: AllocatorData {
                buckets: Buckets {
                    files: buckets.map(File::from),
                    slot_counts,
                    free_lists,
                },
                direct_dir,
            },
        })
    }

    pub fn add(
        &mut self,
        fd: OwnedFd,
        to: RingKind,
        mime_type: &MimeType,
    ) -> Result<AddResponse, CliError> {
        let id = self.add_internal(to, |head, data| data.alloc(fd, mime_type, to, head))?;
        Ok(AddResponse::Success {
            id: composite_id(to, id),
        })
    }

    fn add_internal(
        &mut self,
        to: RingKind,
        alloc: impl FnOnce(u32, &mut AllocatorData) -> Result<Entry, CliError>,
    ) -> Result<u32, CliError> {
        let WritableRing { writer, ring } = &mut self.rings[to];
        let head = ring.write_head();

        if let Some(entry) = ring.get(head) {
            writer.write(Entry::Uninitialized, head)?;
            self.data.free(entry, to, head)?;

            // Only GC on allocation instead of in AllocatorData::free to avoid spamming GCs
            // when removing many entries. This is common in deduplication for example.
            if let Entry::Bucketed(_) = entry {
                self.gc_(u16::MAX.into())?;
            }
        }
        let entry = alloc(head, &mut self.data)?;
        let WritableRing { writer, ring } = &mut self.rings[to];

        writer
            .write(entry, head)
            .map_err(CliError::from)
            .map_err(|e| {
                if let Err(e2) = self.data.free(entry, to, head) {
                    CliError::Multiple(vec![e, e2])
                } else {
                    e
                }
            })?;
        writer.set_write_head(ring.next_head(head))?;
        {
            let len = head + 1;
            if len > ring.len() {
                debug!("Growing {to:?} ring to length {len}.");
                unsafe {
                    ring.set_len(len);
                }
            }
        }

        Ok(head)
    }

    fn get_entry(&self, id: u64) -> Result<(RingKind, u32, Entry), IdNotFoundError> {
        let (ring, id) = decompose_id(id)?;
        let Some(entry) = self.rings[ring].ring.get(id) else {
            return Err(IdNotFoundError::Entry(id));
        };
        Ok((ring, id, entry))
    }

    pub fn move_to_front(
        &mut self,
        id: u64,
        to: Option<RingKind>,
    ) -> Result<MoveToFrontResponse, CliError> {
        let (from, from_id, from_entry) = match self.get_entry(id) {
            Err(e) => return Ok(MoveToFrontResponse::Error(e)),
            Ok((_, from_id, Entry::Uninitialized)) => {
                return Ok(MoveToFrontResponse::Error(IdNotFoundError::Entry(from_id)));
            }
            Ok(r) => r,
        };
        let to = to.unwrap_or(from);
        let WritableRing { writer, ring } = &mut self.rings[from];

        if from == to && ring.next_head(from_id) == ring.write_head() {
            return Ok(MoveToFrontResponse::Success {
                id: composite_id(from, from_id),
            });
        }
        writer.write(Entry::Uninitialized, from_id)?;

        let to_id = self.add_internal(to, |to_id, AllocatorData { ref direct_dir, .. }| {
            debug!(
                "Moving entry {from_entry:?} from {from:?} ring at position {from_id} to {to:?} \
                 ring at position {to_id}."
            );

            match from_entry {
                Entry::Uninitialized => unreachable!(),
                Entry::Bucketed(_) => {
                    // Nothing to do, buckets are shared between rings.
                }
                Entry::File => {
                    let mut from_buf = Default::default();
                    let from_buf = direct_file_name(&mut from_buf, from, from_id);
                    let mut to_buf = Default::default();
                    let to_buf = direct_file_name(&mut to_buf, to, to_id);

                    renameat(direct_dir, &*from_buf, direct_dir, &*to_buf)
                        .map_io_err(|| "Failed to rename direct allocation file.")?;
                }
            }
            Ok(from_entry)
        })?;
        Ok(MoveToFrontResponse::Success {
            id: composite_id(to, to_id),
        })
    }

    #[allow(clippy::similar_names)]
    pub fn swap(&mut self, id1: u64, id2: u64) -> Result<SwapResponse, CliError> {
        let (ring1, id1, entry1) = match self.get_entry(id1) {
            Ok(r) => r,
            Err(e) => {
                return Ok(SwapResponse {
                    error1: Some(e),
                    error2: None,
                });
            }
        };
        let (ring2, id2, entry2) = match self.get_entry(id2) {
            Ok(r) => r,
            Err(e) => {
                return Ok(SwapResponse {
                    error1: None,
                    error2: Some(e),
                });
            }
        };
        if entry1 == Entry::Uninitialized && entry2 == Entry::Uninitialized {
            return Ok(SwapResponse {
                error1: Some(IdNotFoundError::Entry(id1)),
                error2: Some(IdNotFoundError::Entry(id2)),
            });
        }
        debug!(
            "Swapping entry {entry1:?} in {ring1:?} ring at position {id1} with entry {entry2:?} \
             in {ring2:?} ring at position {id2}."
        );

        self.rings[ring1].writer.write(entry2, id1)?;
        self.rings[ring2].writer.write(entry1, id2)?;

        match (entry1, entry2) {
            (Entry::File, _) | (_, Entry::File) => {
                let rings = [ring1, ring2];
                let ids = [id1, id2];
                let from_idx = usize::from(entry1 != Entry::File);
                let to_idx = usize::from(entry1 == Entry::File);

                let mut from_buf = Default::default();
                let from_buf = direct_file_name(&mut from_buf, rings[from_idx], ids[from_idx]);
                let mut to_buf = Default::default();
                let to_buf = direct_file_name(&mut to_buf, rings[to_idx], ids[to_idx]);

                let direct_dir = &self.data.direct_dir;
                let flags = if entry1 == entry2 {
                    RenameFlags::EXCHANGE
                } else {
                    RenameFlags::empty()
                };
                renameat_with(direct_dir, &*from_buf, direct_dir, &*to_buf, flags)
                    .map_io_err(|| "Failed to rename direct allocation file.")?;
            }
            (Entry::Bucketed(_), Entry::Bucketed(_) | Entry::Uninitialized)
            | (Entry::Uninitialized, Entry::Bucketed(_)) => {
                // Nothing to do.
            }
            (Entry::Uninitialized, Entry::Uninitialized) => unreachable!(),
        }

        Ok(SwapResponse {
            error1: None,
            error2: None,
        })
    }

    pub fn remove(&mut self, id: u64) -> Result<RemoveResponse, CliError> {
        let (ring, id, entry) = match self.get_entry(id) {
            Err(e) => return Ok(RemoveResponse { error: Some(e) }),
            Ok((_, id, Entry::Uninitialized)) => {
                return Ok(RemoveResponse {
                    error: Some(IdNotFoundError::Entry(id)),
                });
            }
            Ok(r) => r,
        };
        debug!("Removing entry {entry:?} in {ring:?} ring at position {id}.");

        self.rings[ring].writer.write(Entry::Uninitialized, id)?;
        self.data.free(entry, ring, id)?;

        Ok(RemoveResponse { error: None })
    }

    pub fn gc(&mut self, max_wasted_bytes: u64) -> Result<GarbageCollectResponse, CliError> {
        self.gc_(max_wasted_bytes)
            .map(|bytes_freed| GarbageCollectResponse { bytes_freed })
    }

    #[allow(clippy::too_many_lines)]
    fn gc_(&mut self, max_wasted_bytes: u64) -> Result<u64, CliError> {
        const MIN_BYTES_TO_FREE: u64 = 1 << 14;

        let wasted_bucket_bytes = self
            .data
            .buckets
            .free_lists
            .lists
            .0
            .iter()
            .enumerate()
            .map(|(i, v)| u64::try_from(v.len()).unwrap() * u64::from(bucket_to_length(i)))
            .sum::<u64>();
        debug!(
            "GC requested with {max_wasted_bytes} bytes of max wasted space; found \
             {wasted_bucket_bytes} wasted bytes."
        );
        if wasted_bucket_bytes <= max_wasted_bytes {
            return Ok(0);
        }
        info!("Running GC.");

        let max_wasted_bytes = if wasted_bucket_bytes - max_wasted_bytes < MIN_BYTES_TO_FREE {
            wasted_bucket_bytes.saturating_sub(MIN_BYTES_TO_FREE)
        } else {
            max_wasted_bytes
        };

        let layers_to_remove = {
            let free_slot_counts: [_; NUM_BUCKETS] = {
                let mut a = array::from_fn(|bucket| {
                    (
                        u8::try_from(bucket).unwrap(),
                        u32::try_from(self.data.buckets.free_lists.lists.0[bucket].len()).unwrap(),
                    )
                });
                a.sort_unstable_by_key(|&(_, count)| count);
                a
            };
            let bytes_in_remaining_layers: [_; NUM_BUCKETS] = {
                let mut v = ArrayVec::new_const();

                let mut accum = 0;
                for &(bucket, _) in free_slot_counts.iter().rev() {
                    accum += bucket_to_length(bucket.into());
                    v.push(accum);
                }

                v.into_inner().unwrap()
            };

            let mut layers_to_remove = 0;
            let mut remaining_wasted_bucket_bytes = wasted_bucket_bytes;

            for (i, &(_, free_slots)) in free_slot_counts.iter().enumerate() {
                let available_layers = u64::from(free_slots) - layers_to_remove;
                if available_layers == 0 {
                    continue;
                }
                let bytes_per_layer =
                    u64::from(bytes_in_remaining_layers[free_slot_counts.len() - 1 - i]);

                let layers_used = (remaining_wasted_bucket_bytes - max_wasted_bytes)
                    .div_ceil(bytes_per_layer)
                    .min(available_layers);
                remaining_wasted_bucket_bytes =
                    remaining_wasted_bucket_bytes.saturating_sub(layers_used * bytes_per_layer);
                layers_to_remove += layers_used;

                if remaining_wasted_bucket_bytes <= max_wasted_bytes {
                    break;
                }
            }

            u32::try_from(layers_to_remove).unwrap()
        };
        debug!(
            "Removing {layers_to_remove} layers to achieve {max_wasted_bytes} max wasted bytes."
        );

        let Buckets {
            files,
            slot_counts,
            free_lists,
        } = &mut self.data.buckets;

        let mut swappable_allocations = [const { BinaryHeap::new() }; NUM_BUCKETS];
        {
            let max_swappable_allocations: [_; NUM_BUCKETS] = array::from_fn(|bucket| {
                min(
                    usize::try_from(layers_to_remove).unwrap(),
                    free_lists.lists.0[bucket].len(),
                )
                    // Add one so we can find the truncate pivot point in the free list later
                    + 1
            });
            for (bucket, h) in swappable_allocations.iter_mut().enumerate() {
                h.reserve(max_swappable_allocations[bucket]);
            }

            for free_slots in &mut free_lists.lists.0 {
                free_slots.sort_unstable_by_key(|&index| Reverse(index));
            }

            for kind in [RingKind::Favorites, RingKind::Main] {
                let WritableRing { writer: _, ring } = &self.rings[kind];
                for i in 0..ring.len() {
                    match ring.get(i) {
                        Some(Entry::Bucketed(entry)) => {
                            let bucket = usize::from(size_to_bucket(entry.size()));
                            if entry.index()
                                <= free_lists.lists.0[bucket]
                                    .last()
                                    .copied()
                                    .unwrap_or(u32::MAX)
                            {
                                continue;
                            }

                            let slots = &mut swappable_allocations[bucket];
                            let item = Reverse((entry.index(), RingAndIndex::new(kind, i)));

                            if slots.len() == max_swappable_allocations[bucket] {
                                if item < *slots.peek().unwrap() {
                                    slots.pop();
                                    slots.push(item);
                                }
                            } else {
                                slots.push(item);
                            }
                        }
                        Some(Entry::Uninitialized | Entry::File) | None => continue,
                    }
                }
            }
        }
        let swappable_allocations = swappable_allocations.map(|heap| {
            heap.into_iter()
                .map(|Reverse(item)| item)
                .collect::<BinaryHeap<_>>()
        });

        let mut pending_frees = Vec::with_capacity(usize::try_from(layers_to_remove).unwrap());
        let mut bytes_freed = 0;
        for ((((file, slot_count), free_slots), mut swappable_allocations), bucket_size) in files
            .iter_mut()
            .zip(slot_counts)
            .zip(&mut free_lists.lists.0)
            .zip(swappable_allocations)
            .zip((0..NUM_BUCKETS).map(bucket_to_length))
        {
            let mut swap = || -> Result<_, CliError> {
                for _ in 0..layers_to_remove {
                    let Some(&free) = free_slots.last() else {
                        break;
                    };
                    let Some(&(alloc, rai)) = swappable_allocations.peek() else {
                        break;
                    };
                    if free >= alloc {
                        break;
                    }

                    let WritableRing { writer, ring } = &mut self.rings[rai.ring()];
                    let size = match ring.get(rai.index()) {
                        Some(Entry::Bucketed(entry)) => entry.size(),
                        _ => unreachable!(),
                    };
                    trace!(
                        "Replacing free slot {free} with allocated slot {alloc} for bucket of \
                         length {bucket_size}."
                    );

                    copy_file_range_all(
                        &*file,
                        Some(&mut (u64::from(alloc) * u64::from(bucket_size))),
                        &*file,
                        Some(&mut (u64::from(free) * u64::from(bucket_size))),
                        // Copy the NUL byte too
                        if size < bucket_size { size + 1 } else { size }.into(),
                    )
                    .map_io_err(|| {
                        format!(
                            "Failed to copy bucket slot {alloc} to {free} in bucket {}.",
                            size_to_bucket(size)
                        )
                    })?;
                    writer.write(
                        Entry::Bucketed(InitializedEntry::bucket(size, free)),
                        rai.index(),
                    )?;

                    free_slots.pop();
                    swappable_allocations.pop();
                    pending_frees.push(alloc);
                }
                Ok(())
            };

            {
                let r = swap();
                free_slots.append(&mut pending_frees);
                r?;
            }

            let drop_count =
                if let Some(&(highest_allocated_bucket_index, _)) = swappable_allocations.peek() {
                    free_slots.sort_unstable_by_key(|&index| Reverse(index));
                    let (Ok(pivot) | Err(pivot)) = free_slots
                        .binary_search_by_key(&Reverse(highest_allocated_bucket_index), |&index| {
                            Reverse(index)
                        });
                    pivot
                } else {
                    free_slots.len()
                };
            if drop_count == 0 {
                continue;
            }
            debug!("Dropping last {drop_count} slots for bucket of size {bucket_size}.");

            ftruncate(
                file,
                (u64::from(*slot_count) - u64::try_from(drop_count).unwrap())
                    * u64::from(bucket_size),
            )
            .map_io_err(|| {
                format!("Failed to truncate bucket file with bucket size {bucket_size}.")
            })?;
            *slot_count -= u32::try_from(drop_count).unwrap();
            free_slots.drain(..drop_count);
            bytes_freed += u64::try_from(drop_count).unwrap() * u64::from(bucket_size);
        }
        info!("GC freed {bytes_freed} bytes.");
        Ok(bytes_freed)
    }

    pub fn shutdown(mut self) -> Result<(), CliError> {
        self.data.buckets.free_lists.save()
    }
}

impl AllocatorData {
    fn alloc(
        &mut self,
        data: OwnedFd,
        mime_type: &MimeType,
        to: RingKind,
        id: u32,
    ) -> Result<Entry, CliError> {
        debug!("Allocating entry to {to:?} ring at position {id} with mime type {mime_type:?}.");
        let mut received = File::from(
            openat(
                &self.direct_dir,
                c".",
                OFlags::RDWR | OFlags::TMPFILE,
                Mode::RUSR | Mode::WUSR,
            )
            .map_io_err(|| "Failed to create data receiver file.")?,
        );

        let size = io::copy(&mut File::from(data), &mut received)
            .map_io_err(|| "Failed to copy data to receiver file.")?;
        debug!("Received {size} bytes.");

        if TEXT_MIMES.iter().any(|b| mime_type.eq_ignore_ascii_case(b)) {
            if size > 0 && size < 4096 {
                self.alloc_bucket(received, u16::try_from(size).unwrap())
            } else {
                self.alloc_direct(received, &MimeType::new(), to, id)
            }
        } else {
            self.alloc_direct(received, mime_type, to, id)
        }
    }

    fn alloc_bucket(&mut self, data: File, size: u16) -> Result<Entry, CliError> {
        debug!("Allocating {size} byte bucket slot.");
        let bucket = usize::from(size_to_bucket(size));
        let Buckets {
            files,
            slot_counts: bucket_lengths,
            free_lists,
        } = &mut self.buckets;

        let free_bucket = free_lists.alloc(bucket);
        let bucket_index = free_bucket
            .as_ref()
            .map_or_else(|| bucket_lengths[bucket], |g| g.id);
        let bucket_len = bucket_to_length(bucket);

        debug!("Writing to bucket {bucket} at slot {bucket_index}.");
        {
            let grow = free_bucket.is_none();
            if grow {
                bucket_lengths[bucket] += 1;
            }

            let mut offset = u64::from(bucket_index) * u64::from(bucket_len);
            copy_file_range_all(
                data,
                Some(&mut 0),
                &files[bucket],
                Some(&mut offset),
                usize::from(size),
            )
            .map_io_err(|| format!("Failed to copy data to bucket {bucket}."))?;
            if size < bucket_len {
                files[bucket]
                    .write_all_at(
                        &[0],
                        if grow {
                            u64::from(bucket_index + 1) * u64::from(bucket_len) - 1
                        } else {
                            offset
                        },
                    )
                    .map_io_err(|| format!("Failed to write NUL bytes to bucket {bucket}."))?;
            }
        }

        let entry = InitializedEntry::bucket(size, bucket_index);
        free_bucket.map(BucketSlotGuard::into_inner);
        Ok(Entry::Bucketed(entry))
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn alloc_direct(
        &mut self,
        data: File,
        &mime_type: &MimeType,
        to: RingKind,
        id: u32,
    ) -> Result<Entry, CliError> {
        const _: () = assert!(size_of::<RingKind>() <= u8::BITS as usize);
        debug!("Allocating direct entry.");

        if !mime_type.is_empty() {
            fsetxattr(
                &data,
                c"user.mime_type",
                mime_type.as_bytes(),
                XattrFlags::CREATE,
            )
            .map_io_err(|| "Failed to create mime type attribute.")?;
        }

        let mut buf = Default::default();
        let buf = direct_file_name(&mut buf, to, id);
        link_tmp_file(data, &self.direct_dir, &*buf)
            .map_io_err(|| "Failed to materialize direct allocation.")?;

        Ok(Entry::File)
    }

    fn free(&mut self, entry: Entry, to: RingKind, id: u32) -> Result<(), CliError> {
        debug!("Freeing entry in {to:?} ring at position {id}: {entry:?}");
        match entry {
            Entry::Uninitialized => Ok(()),
            Entry::Bucketed(bucket) => {
                self.buckets
                    .free_lists
                    .free(size_to_bucket(bucket.size()).into(), bucket.index());
                Ok(())
            }
            Entry::File => self.free_direct(to, id),
        }
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn free_direct(&mut self, to: RingKind, id: u32) -> Result<(), CliError> {
        debug!("Freeing direct allocation.");
        let mut buf = Default::default();
        let buf = direct_file_name(&mut buf, to, id);
        unlinkat(&self.direct_dir, &*buf, AtFlags::empty())
            .map_io_err(|| "Failed to remove direct allocation file.")?;
        Ok(())
    }
}
