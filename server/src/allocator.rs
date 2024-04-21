use std::{
    cmp::max,
    ffi::CStr,
    fmt::Debug,
    fs,
    fs::File,
    io,
    io::{ErrorKind, IoSlice, Read, Write},
    marker::PhantomData,
    mem,
    mem::ManuallyDrop,
    ops::{Deref, Index, IndexMut},
    os::{fd::OwnedFd, unix::fs::FileExt},
    path::{Path, PathBuf},
    slice,
};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use log::{debug, info};
use ringboard_core::{
    copy_file_range_all,
    protocol::{
        composite_id, AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, RemoveResponse,
        RingKind, SwapResponse,
    },
    ring,
    ring::{entries_to_offset, BucketEntry, Entry, Header, RawEntry, Ring},
    IoErr, PathView, StringView,
};
use rustix::{
    fs::{
        fsetxattr, openat, renameat, renameat_with, statx, unlinkat, AtFlags, Mode, OFlags,
        RenameFlags, StatxFlags, XattrFlags, CWD,
    },
    path::Arg,
};

use crate::{
    utils::{link_tmp_file, TEXT_MIMES},
    CliError,
};

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

    fn write(&mut self, entry: Entry, at: u32) -> ringboard_core::Result<()> {
        debug!("Writing entry to position {at}: {entry:?}");
        self.ring
            .write_all_at(&RawEntry::from(entry).to_le_bytes(), entries_to_offset(at))
            .map_io_err(|| format!("Failed to write entry to Ringboard database: {entry:?}"))
    }

    fn set_write_head(&mut self, head: u32) -> ringboard_core::Result<()> {
        debug!("Setting write head to {head}.");
        self.ring
            .write_all_at(
                &head.to_le_bytes(),
                u64::try_from(ring::MAGIC.len() + mem::size_of_val(&ring::VERSION)).unwrap(),
            )
            .map_io_err(|| format!("Failed to update Ringboard write head: {head}"))
    }
}

struct WritableRing {
    writer: RingWriter,
    ring: Ring,
}

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

pub struct Allocator {
    rings: Rings,
    data: AllocatorData,
}

struct AllocatorData {
    buckets: Buckets,
    direct_dir: OwnedFd,

    cache: bitcode::Buffer,
}

struct Buckets {
    files: [File; 11],
    slot_counts: [u32; 11],
    free_lists: FreeLists,
}

struct FreeLists {
    lists: RawFreeLists,
    file: File,
}

#[derive(Encode, Decode, Default)]
struct RawFreeLists([Vec<u32>; 11]);

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
    fn load<P: AsRef<Path>>(path: P) -> Result<Self, CliError> {
        let path = path.as_ref();
        let mut file = match openat(CWD, path, OFlags::RDWR, Mode::empty()) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let file = File::from(
                    openat(
                        CWD,
                        path,
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

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_io_err(|| format!("Failed to read free lists: {path:?}"))?;

        if bytes.is_empty() {
            todo!("start recovery")
        } else {
            let lists = bitcode::decode(&bytes).map_err(|e| CliError::DeserializeError {
                error: e,
                context: format!("Free lists file: {path:?}").into(),
            })?;
            file.set_len(0)
                .map_io_err(|| format!("Failed to truncate free lists: {path:?}"))?;
            Ok(Self { lists, file })
        }
    }

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
    pub fn open(mut data_dir: PathBuf, max_entries: u32) -> Result<Self, CliError> {
        let mut open_ring = |name| -> Result<_, CliError> {
            let main = PathView::new(&mut data_dir, name);
            Ok(WritableRing {
                writer: RingWriter::open(&*main)?,
                ring: Ring::open(max_entries, &*main)?,
            })
        };
        let main_ring = open_ring("main.ring")?;
        let favorites_ring = open_ring("favorites.ring")?;

        let mut create_dir = |name| {
            let dir = PathView::new(&mut data_dir, name);
            fs::create_dir_all(&dir).map_io_err(|| format!("Failed to create directory: {dir:?}"))
        };
        create_dir("direct")?;
        create_dir("buckets")?;

        let buckets = {
            let mut buckets = PathView::new(&mut data_dir, "buckets");
            let mut fds = ArrayVec::new_const();
            let mut buf = String::with_capacity("(1024, 2048]".len());

            let mut open = |name: &str| -> Result<_, CliError> {
                let file = PathView::new(&mut buckets, name);
                fds.push(File::from(
                    openat(
                        CWD,
                        &*file,
                        OFlags::WRONLY | OFlags::CREATE,
                        Mode::RUSR | Mode::WUSR,
                    )
                    .map_io_err(|| format!("Failed to create bucket: {file:?}"))?,
                ));
                Ok(())
            };

            open("(0, 4]")?;
            for end in 3..12 {
                use std::fmt::Write;

                let start = end - 1;

                let mut buf = StringView::new(&mut buf);
                write!(buf, "({}, {}]", 1 << start, 1 << end).unwrap();
                open(&buf)?;
            }
            open("(2048, 4096)")?;

            fds.into_inner().unwrap()
        };
        let slot_counts = {
            let mut counts = ArrayVec::new_const();

            for (i, bucket) in buckets.iter().enumerate() {
                counts.push(
                    u32::try_from(
                        statx(bucket, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                            .map_io_err(|| "Failed to statx bucket.")?
                            .stx_size
                            >> (i + 2),
                    )
                    .unwrap(),
                );
            }

            counts.into_inner().unwrap()
        };

        let direct_dir = {
            let file = PathView::new(&mut data_dir, "direct");
            openat(CWD, &*file, OFlags::DIRECTORY | OFlags::PATH, Mode::empty())
                .map_io_err(|| format!("Failed to open directory: {file:?}"))
        }?;

        let free_lists = FreeLists::load(PathView::new(&mut data_dir, "free-lists"))?;

        Ok(Self {
            rings: Rings([favorites_ring, main_ring]),
            data: AllocatorData {
                buckets: Buckets {
                    files: buckets,
                    slot_counts,
                    free_lists,
                },
                direct_dir,

                cache: bitcode::Buffer::new(),
            },
        })
    }

    pub fn trim(&mut self) {
        mem::take(&mut self.data.cache);
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
            // TODO get rid of this write on the happy path
            writer.write(Entry::Uninitialized, head)?;
            self.data.free(entry, to, head)?;
        }
        let entry = alloc(head, &mut self.data)?;

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

        if TEXT_MIMES.contains(&mime_type.as_str()) {
            if size > 0 && size < 4096 {
                self.alloc_bucket(received, u32::try_from(size).unwrap())
            } else {
                self.alloc_direct(received, mime_type, to, id)
            }
        } else {
            self.alloc_direct(received, mime_type, to, id)
        }
    }

    fn alloc_bucket(&mut self, data: File, size: u32) -> Result<Entry, CliError> {
        debug!("Allocating {size} byte bucket slot.");
        let bucket = size_to_bucket(size);
        let Buckets {
            files,
            slot_counts: bucket_lengths,
            free_lists,
        } = &mut self.buckets;

        let free_bucket = free_lists.alloc(bucket);
        let bucket_index = free_bucket
            .as_ref()
            .map_or_else(|| bucket_lengths[bucket], |g| g.id);
        let bucket_len = 1 << (bucket + 2);

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
                usize::try_from(size).unwrap(),
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

        let entry = BucketEntry::new(size, bucket_index).unwrap();
        free_bucket.map(BucketSlotGuard::into_inner);
        Ok(Entry::Bucketed(entry))
    }

    fn alloc_direct(
        &mut self,
        data: File,
        &mime_type: &MimeType,
        to: RingKind,
        id: u32,
    ) -> Result<Entry, CliError> {
        const _: () = assert!(mem::size_of::<RingKind>() <= u8::BITS as usize);
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
                    .free(size_to_bucket(bucket.size()), bucket.index());
                Ok(())
            }
            Entry::File => self.free_direct(to, id),
        }
    }

    fn free_direct(&mut self, to: RingKind, id: u32) -> Result<(), CliError> {
        debug!("Freeing direct allocation.");
        let mut buf = Default::default();
        let buf = direct_file_name(&mut buf, to, id);
        unlinkat(&self.direct_dir, &*buf, AtFlags::empty())
            .map_io_err(|| "Failed to remove direct allocation file.")?;
        Ok(())
    }
}

fn size_to_bucket(size: u32) -> usize {
    usize::try_from(max(1, size.saturating_sub(1)).ilog2().saturating_sub(1)).unwrap()
}

struct DirectFileNameToken<'a, T>(&'a mut [u8], PhantomData<T>);

impl<T> Deref for DirectFileNameToken<'_, T> {
    type Target = CStr;

    fn deref(&self) -> &Self::Target {
        unsafe { CStr::from_ptr(self.0.as_ptr().cast()) }
    }
}

fn direct_file_name(
    buf: &mut [u8; "256".len() + 1 + "4294967296".len() + 1],
    to: RingKind,
    id: u32,
) -> DirectFileNameToken<()> {
    write!(buf.as_mut_slice(), "{:0>3}_{id:0>10}\0", to as u8).unwrap();
    DirectFileNameToken(buf.as_mut_slice(), PhantomData)
}

pub fn decompose_id(id: u64) -> Result<(RingKind, u32), IdNotFoundError> {
    match id >> 32 {
        0 => Ok(RingKind::Favorites),
        1 => Ok(RingKind::Main),
        ring => Err(IdNotFoundError::Ring(u32::try_from(ring).unwrap())),
    }
    .map(|ring| (ring, u32::try_from(id & u64::from(u32::MAX)).unwrap()))
}
