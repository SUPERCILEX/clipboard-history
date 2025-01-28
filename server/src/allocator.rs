use std::{
    array,
    cmp::{Reverse, min},
    collections::BinaryHeap,
    ffi::CStr,
    fmt::Debug,
    fs::File,
    io,
    io::{ErrorKind, ErrorKind::AlreadyExists, IoSlice, Read, Seek, SeekFrom, Write},
    mem,
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Index, IndexMut},
    os::{fd::OwnedFd, unix::fs::FileExt},
    slice,
};

use arrayvec::{ArrayString, ArrayVec};
use bitcode::{Decode, Encode};
use bitvec::{order::Lsb0, vec::BitVec};
use log::{debug, error, info, trace, warn};
use ringboard_core::{
    IoErr, NUM_BUCKETS, RingAndIndex, bucket_to_length, copy_file_range_all, create_tmp_file,
    direct_file_name, is_plaintext_mime, link_tmp_file, open_buckets,
    protocol::{
        AddResponse, GarbageCollectResponse, IdNotFoundError, MimeType, MoveToFrontResponse,
        RemoveResponse, RingKind, SwapResponse, composite_id, decompose_id,
    },
    ring,
    ring::{Entry, Header, InitializedEntry, RawEntry, Ring, entries_to_offset},
    size_to_bucket,
};
use rustix::{
    fs::{
        AtFlags, CWD, Mode, OFlags, RenameFlags, XattrFlags, fsetxattr, ftruncate, getxattr, mkdir,
        openat, renameat, renameat_with, unlinkat,
    },
    io::Errno,
    path::Arg,
};

use crate::CliError;

#[derive(Debug)]
struct RingWriter {
    ring: File,
}

impl RingWriter {
    fn open<P: Arg + Copy + Debug>(path: P) -> Result<Self, CliError> {
        let ring = match openat(CWD, path, OFlags::RDWR, Mode::empty()) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let fd = openat(
                    CWD,
                    path,
                    OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
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
    metadata_dir: Option<OwnedFd>,
    scratchpad: File,
    tmp_file_unsupported: bool,
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
}

#[derive(Encode, Decode, Default, Debug)]
struct RawFreeLists([Vec<u32>; NUM_BUCKETS]);

struct BucketSlotGuard<'a> {
    id: u32,
    free_list: &'a mut Vec<u32>,
}

impl BucketSlotGuard<'_> {
    // TODO https://github.com/rust-lang/rust-clippy/issues/14091
    #[allow(clippy::missing_const_for_fn)]
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
    fn load(rings: &Rings) -> Result<Self, CliError> {
        let mut file = match openat(CWD, c"free-lists", OFlags::RDWR, Mode::empty()) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(Self {
                    lists: RawFreeLists::default(),
                });
            }
            r => File::from(r.map_io_err(|| "Failed to open free lists file.")?),
        };

        {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_io_err(|| "Failed to read free lists file.")?;

            if !bytes.is_empty() {
                file.set_len(0)
                    .map_io_err(|| "Failed to truncate free lists file.")?;
                match bitcode::decode(&bytes) {
                    Ok(lists) => return Ok(Self { lists }),
                    Err(e) => {
                        error!("Corrupted free lists file.\nError: {e:?}");
                    }
                }
            }
        }
        warn!("Reconstructing allocator free lists.");

        let mut allocations = [BitVec::<usize, Lsb0>::EMPTY; NUM_BUCKETS];
        for ring in [RingKind::Favorites, RingKind::Main] {
            let ring = &rings[ring].ring;
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
                    Entry::Uninitialized | Entry::File => (),
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
        })
    }

    fn save(&self) -> Result<(), CliError> {
        info!("Saving allocator free list to disk.");
        let file = openat(
            CWD,
            c"free-lists",
            OFlags::WRONLY | OFlags::CREATE,
            Mode::RUSR | Mode::WUSR,
        )
        .map_io_err(|| "Failed to open free lists file.")?;
        let bytes = bitcode::encode(&self.lists);
        debug_assert!(!bytes.is_empty());
        File::from(file)
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

fn create_scratchpad(tmp_file_unsupported: &mut bool) -> ringboard_core::Result<File> {
    create_tmp_file(
        tmp_file_unsupported,
        CWD,
        c".",
        c".scratchpad",
        OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_io_err(|| "Failed to create scratchpad file.")
}

impl Allocator {
    pub fn open() -> Result<Self, CliError> {
        let open_ring = |kind: RingKind| -> Result<_, CliError> {
            let writer = RingWriter::open(kind.file_name_cstr())?;
            Ok(WritableRing {
                ring: Ring::open_fd(kind.default_max_entries(), &writer.ring)?,
                writer,
            })
        };
        let main_ring = open_ring(RingKind::Main)?;
        let favorites_ring = open_ring(RingKind::Favorites)?;

        let create_dir = |name| match mkdir(name, Mode::RWXU) {
            Err(e) if e.kind() == AlreadyExists => Ok(()),
            r => r.map_io_err(|| format!("Failed to create directory: {name:?}")),
        };
        create_dir(c"direct")?;
        create_dir(c"buckets")?;

        let xattr_unsupported = matches!(
            getxattr(c"direct", c"user.mime_type", &mut []),
            Err(Errno::NOTSUP)
        );
        if xattr_unsupported {
            create_dir(c"metadata")?;
        }

        let (buckets, slot_counts) = {
            let mut path = ArrayString::<{ "buckets/(1024, 2048]".len() + 1 }>::new_const();
            path.push_str("buckets/");
            open_buckets(|name| {
                path.truncate("buckets/".len());
                path.push_str(name);
                path.push(char::from(0));
                openat(
                    CWD,
                    unsafe { CStr::from_ptr(path.as_ptr().cast()) },
                    OFlags::RDWR | OFlags::CREATE,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_io_err(|| format!("Failed to create bucket: {path:?}"))
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

        let open_dir = |name| {
            openat(CWD, name, OFlags::DIRECTORY | OFlags::PATH, Mode::empty())
                .map_io_err(|| format!("Failed to open directory: {name:?}"))
        };
        let direct_dir = open_dir(c"direct")?;
        let metadata_dir = if xattr_unsupported {
            Some(open_dir(c"metadata")?)
        } else {
            None
        };

        let rings = Rings([favorites_ring, main_ring]);
        let free_lists = FreeLists::load(&rings)?;
        let mut tmp_file_unsupported = false;
        let scratchpad = create_scratchpad(&mut tmp_file_unsupported)?;

        Ok(Self {
            rings,
            data: AllocatorData {
                buckets: Buckets {
                    files: buckets.map(File::from),
                    slot_counts,
                    free_lists,
                },
                direct_dir,
                metadata_dir,
                scratchpad,
                tmp_file_unsupported,
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

        let run = |to_id,
                   &mut AllocatorData {
                       ref direct_dir,
                       ref metadata_dir,
                       ..
                   }: &mut AllocatorData| {
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
                    let mut from_file_name = [MaybeUninit::uninit(); 14];
                    let from_file_name = direct_file_name(&mut from_file_name, from, from_id);
                    let mut to_file_name = [MaybeUninit::uninit(); 14];
                    let to_file_name = direct_file_name(&mut to_file_name, to, to_id);

                    renameat(direct_dir, from_file_name, direct_dir, to_file_name).map_io_err(
                        || {
                            format!(
                                "Failed to rename direct allocation file from {from_file_name:?} \
                                 to {to_file_name:?}."
                            )
                        },
                    )?;
                    if let Some(metadata_dir) = metadata_dir {
                        renameat(metadata_dir, from_file_name, metadata_dir, to_file_name)
                            .map_io_err(|| {
                                format!(
                                    "Failed to rename metadata file from {from_file_name:?} to \
                                     {to_file_name:?}."
                                )
                            })
                            .map_err(CliError::from)
                            .map_err(|e| {
                                if let Err(e2) =
                                    renameat(direct_dir, to_file_name, direct_dir, from_file_name)
                                        .map_io_err(|| {
                                            format!(
                                                "Failed to undo renaming direct allocation file \
                                                 from {to_file_name:?} to {from_file_name:?}."
                                            )
                                        })
                                        .map_err(CliError::from)
                                {
                                    CliError::Multiple(vec![e, e2])
                                } else {
                                    e
                                }
                            })?;
                    }
                }
            }
            Ok(from_entry)
        };
        let to_id = self.add_internal(to, run)?;
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

                let mut from_file_name = [MaybeUninit::uninit(); 14];
                let from_file_name =
                    direct_file_name(&mut from_file_name, rings[from_idx], ids[from_idx]);
                let mut to_file_name = [MaybeUninit::uninit(); 14];
                let to_file_name = direct_file_name(&mut to_file_name, rings[to_idx], ids[to_idx]);

                let direct_dir = &self.data.direct_dir;
                let flags = if entry1 == entry2 {
                    RenameFlags::EXCHANGE
                } else {
                    RenameFlags::empty()
                };
                renameat_with(direct_dir, from_file_name, direct_dir, to_file_name, flags)
                    .map_io_err(|| {
                        format!(
                            "Failed to swap direct allocation files between {from_file_name:?} \
                             and {to_file_name:?}."
                        )
                    })?;
                if let Some(metadata_dir) = &self.data.metadata_dir {
                    renameat_with(
                        metadata_dir,
                        from_file_name,
                        metadata_dir,
                        to_file_name,
                        flags,
                    )
                    .map_io_err(|| {
                        format!(
                            "Failed to swap metadata files between {from_file_name:?} and \
                             {to_file_name:?}."
                        )
                    })
                    .map_err(CliError::from)
                    .map_err(|e| {
                        if let Err(e2) = renameat_with(
                            direct_dir,
                            to_file_name,
                            direct_dir,
                            from_file_name,
                            flags,
                        )
                        .map_io_err(|| {
                            format!(
                                "Failed to undo swapping direct allocation files between \
                                 {to_file_name:?} and {from_file_name:?}."
                            )
                        })
                        .map_err(CliError::from)
                        {
                            CliError::Multiple(vec![e, e2])
                        } else {
                            e
                        }
                    })?;
                }
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
                        Some(Entry::Uninitialized | Entry::File) | None => (),
                    }
                }
            }
        }
        let swappable_allocations = swappable_allocations.map(|heap| {
            let mut allocs = heap.into_vec();
            allocs.sort_unstable_by_key(|&Reverse(e)| e);
            allocs
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
                    let Some(&Reverse((alloc, rai))) = swappable_allocations.last() else {
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

            let drop_count = if let Some(&Reverse((highest_allocated_bucket_index, _))) =
                swappable_allocations.last()
            {
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
            drop(swappable_allocations);
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

    pub fn shutdown(self) -> Result<(), CliError> {
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

        let size = io::copy(&mut File::from(data), &mut self.scratchpad)
            .map_io_err(|| "Failed to copy data to receiver file.")?;
        debug!("Received {size} bytes.");

        if is_plaintext_mime(mime_type) {
            if size > 0 && size < 4096 {
                self.alloc_bucket(u16::try_from(size).unwrap())
            } else {
                self.alloc_direct(size, &MimeType::new_const(), to, id)
            }
        } else {
            self.alloc_direct(size, mime_type, to, id)
        }
    }

    fn alloc_bucket(&mut self, size: u16) -> Result<Entry, CliError> {
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
                &self.scratchpad,
                Some(&mut 0),
                &files[bucket],
                Some(&mut offset),
                usize::from(size),
            )
            .map_io_err(|| format!("Failed to copy data to bucket {bucket}."))?;
            self.scratchpad
                .seek(SeekFrom::Start(0))
                .map_io_err(|| "Failed to reset scratchpad file offset.")?;
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

    fn alloc_direct(
        &mut self,
        size: u64,
        &mime_type: &MimeType,
        to: RingKind,
        id: u32,
    ) -> Result<Entry, CliError> {
        const _: () = assert!(size_of::<RingKind>() <= u8::BITS as usize);
        debug!("Allocating direct entry.");

        if size < 4096 - 1 {
            ftruncate(&self.scratchpad, size).map_io_err(|| "Failed to trim scratchpad file.")?;
        }

        let data = mem::replace(
            &mut self.scratchpad,
            create_scratchpad(&mut self.tmp_file_unsupported)?,
        );
        let mut file_name = [MaybeUninit::uninit(); 14];
        let file_name = direct_file_name(&mut file_name, to, id);

        if let Some(metadata_dir) = &self.metadata_dir {
            let mut metadata = File::from(
                openat(
                    metadata_dir,
                    file_name,
                    OFlags::CREATE | OFlags::WRONLY,
                    Mode::RUSR,
                )
                .map_io_err(|| format!("Failed to create direct metadata file: {file_name:?}"))?,
            );
            if !mime_type.is_empty() {
                metadata.write_all(mime_type.as_bytes()).map_io_err(|| {
                    format!("Failed to write to direct metadata file: {file_name:?}")
                })?;
            }
        } else if !mime_type.is_empty() {
            fsetxattr(
                &data,
                c"user.mime_type",
                mime_type.as_bytes(),
                XattrFlags::CREATE,
            )
            .map_io_err(|| "Failed to create mime type attribute.")?;
        }

        link_tmp_file(data, &self.direct_dir, file_name)
            .map_io_err(|| format!("Failed to materialize direct allocation: {file_name:?}"))
            .map_err(CliError::from)
            .map_err(|e| {
                if let Some(metadata_dir) = &self.metadata_dir
                    && let Err(e2) = unlinkat(metadata_dir, file_name, AtFlags::empty())
                        .map_io_err(|| {
                            format!(
                                "Failed to remove metadata file after materialization failure: \
                                 {file_name:?}"
                            )
                        })
                        .map_err(CliError::from)
                {
                    CliError::Multiple(vec![e, e2])
                } else {
                    e
                }
            })?;

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

    fn free_direct(&self, to: RingKind, id: u32) -> Result<(), CliError> {
        debug!("Freeing direct allocation.");

        let mut file_name = [MaybeUninit::uninit(); 14];
        let file_name = direct_file_name(&mut file_name, to, id);

        unlinkat(&self.direct_dir, file_name, AtFlags::empty())
            .map_io_err(|| format!("Failed to remove direct allocation file: {file_name:?}"))?;
        if let Some(metadata_dir) = &self.metadata_dir {
            unlinkat(metadata_dir, file_name, AtFlags::empty())
                .map_io_err(|| format!("Failed to remove metadata file: {file_name:?}"))?;
        }

        Ok(())
    }
}
