use std::{
    cmp::max,
    convert::Infallible,
    ffi::CStr,
    fs,
    fs::File,
    io,
    io::{ErrorKind, Read, Write},
    marker::PhantomData,
    mem,
    mem::ManuallyDrop,
    ops::{Deref, Index, IndexMut},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::FileExt,
    },
    path::{Path, PathBuf},
};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use clipboard_history_core::{
    protocol::{
        composite_id, AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, RingKind,
    },
    ring::{BucketEntry, Entry, Ring, RingWriter},
    IoErr,
};
use log::{debug, info};
use rustix::fs::{
    copy_file_range, openat, renameat, statx, unlinkat, AtFlags, Mode, OFlags, StatxFlags, CWD,
};

use crate::{
    utils::{link_tmp_file, TEXT_MIMES},
    views::{PathView, StringView},
    CliError,
};

struct WritableRing {
    writer: LoggingRingWriter,
    ring: Ring,
}

struct LoggingRingWriter(RingWriter);

impl LoggingRingWriter {
    pub fn write(&mut self, entry: Entry, at: u32) -> clipboard_history_core::Result<()> {
        debug!("Writing entry to position {at}: {entry:?}");
        self.0.write(entry, at)
    }

    pub fn set_write_head(&mut self, head: u32) -> clipboard_history_core::Result<()> {
        debug!("Setting write head to {head}.");
        self.0.set_write_head(head)
    }
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
    buckets: [File; 11],
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
        let bytes = bitcode::encode(&self.lists).map_err(|error| CliError::SerializeError {
            error,
            context: "Free lists internal error".into(),
        })?;
        self.file
            .write_all_at(&bytes, 0)
            .map_io_err(|| "Failed to write free lists.")?;
        Ok(())
    }

    fn alloc(&mut self, bucket: usize) -> Option<BucketSlotGuard> {
        let free_list = &mut self.lists.0[bucket];
        free_list.pop().map(|id| BucketSlotGuard { free_list, id })
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
                writer: LoggingRingWriter(RingWriter::open(&*main)?),
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
            rings: Rings([main_ring, favorites_ring]),
            data: AllocatorData {
                buckets: Buckets {
                    buckets,
                    slot_counts,
                    free_lists,
                },
                direct_dir,

                cache: bitcode::Buffer::with_capacity(32),
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
        if head > ring.len() {
            debug!("Growing {to:?} ring to length {head}.");
            unsafe {
                ring.set_len(head);
            }
        }

        Ok(head)
    }

    pub fn move_to_front(
        &mut self,
        id: u64,
        to: Option<RingKind>,
    ) -> Result<MoveToFrontResponse, CliError> {
        let (from, from_id) = match decompose_id(id) {
            Ok(r) => r,
            Err(e) => return Ok(MoveToFrontResponse::Error(e)),
        };
        let to = to.unwrap_or(from);

        let WritableRing { writer, ring } = &mut self.rings[from];
        let Some(from_entry) = ring.get(from_id) else {
            return Ok(MoveToFrontResponse::Error(IdNotFoundError::Entry(from_id)));
        };
        if from_entry == Entry::Uninitialized {
            return Ok(MoveToFrontResponse::Error(IdNotFoundError::Entry(from_id)));
        }

        if from == to && ring.next_head(from_id) == ring.write_head() {
            return Ok(MoveToFrontResponse::Success {
                id: composite_id(from, from_id),
            });
        }
        writer.write(Entry::Uninitialized, from_id)?;

        let to_id = self.add_internal(to, |to_id, data| {
            match from_entry {
                Entry::Uninitialized => unreachable!(),
                Entry::Bucketed(_) => {
                    // Nothing to do, buckets are shared between rings.
                }
                Entry::File => {
                    debug!(
                        "Moving direct allocation from {from:?} ring at position {from_id} to \
                         {to:?} ring at position {to_id}."
                    );

                    let mut from_buf = Default::default();
                    let from_buf = direct_metadata_file_name(&mut from_buf, from, from_id);
                    let mut to_buf = Default::default();
                    let to_buf = direct_metadata_file_name(&mut to_buf, to, to_id);

                    renameat(&data.direct_dir, &*from_buf, &data.direct_dir, &*to_buf)
                        .map_io_err(|| "Failed to rename direct allocation metadata file.")?;
                    renameat(
                        &data.direct_dir,
                        &*direct_file_name(from_buf),
                        &data.direct_dir,
                        &*direct_file_name(to_buf),
                    )
                    .map_io_err(|| "Failed to rename direct allocation file.")?;
                }
            }
            Ok(from_entry)
        })?;
        Ok(MoveToFrontResponse::Success {
            id: composite_id(to, to_id),
        })
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
                Mode::RUSR,
            )
            .map_io_err(|| "Failed to create data receiver file.")?,
        );

        io::copy(&mut File::from(data), &mut received)
            .map_io_err(|| "Failed to copy data to receiver file.")?;

        if TEXT_MIMES.contains(&mime_type.as_str()) {
            let size = statx(&received, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                .map_io_err(|| "Failed to statx received data file.")?
                .stx_size;

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
            buckets,
            slot_counts: bucket_lengths,
            free_lists,
        } = &mut self.buckets;

        let free_bucket = free_lists.alloc(bucket);
        let bucket_index = free_bucket
            .as_ref()
            .map(|g| g.id)
            .unwrap_or_else(|| bucket_lengths[bucket]);
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
                &buckets[bucket],
                Some(&mut offset),
                usize::try_from(size).unwrap(),
            )
            .map_io_err(|| format!("Failed to copy data to bucket {bucket}."))?;
            if size < bucket_len {
                buckets[bucket]
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
        debug!("Allocating direct entry.");
        const _: () = assert!(mem::size_of::<RingKind>() <= u8::BITS as usize);
        let mut buf = Default::default();
        let buf = direct_metadata_file_name(&mut buf, to, id);

        {
            let mut file = File::from(
                openat(
                    &self.direct_dir,
                    &*buf,
                    OFlags::WRONLY | OFlags::CREATE,
                    Mode::RUSR,
                )
                .map_io_err(|| "Failed to create direct metadata.")?,
            );

            // TODO write the API for this
            #[derive(Encode, Decode)]
            struct Metadata {
                #[bitcode(with_serde)]
                mime_type: MimeType,
            }

            let bytes = self
                .cache
                .encode(&Metadata { mime_type })
                .map_err(|error| CliError::SerializeError {
                    error,
                    context: "Direct allocation metadata internal error.".into(),
                })?;
            file.write_all(bytes)
                .map_io_err(|| "Failed to write to direct metadata file.")?;
        }

        link_tmp_file(data, &self.direct_dir, &*direct_file_name(buf))
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
        let buf = direct_metadata_file_name(&mut buf, to, id);
        unlinkat(&self.direct_dir, &*buf, AtFlags::empty())
            .map_io_err(|| "Failed to remove direct metadata file.")?;
        unlinkat(&self.direct_dir, &*direct_file_name(buf), AtFlags::empty())
            .map_io_err(|| "Failed to remove direct allocation file.")?;
        Ok(())
    }
}

fn size_to_bucket(size: u32) -> usize {
    usize::try_from(max(1, size.saturating_sub(1)).ilog2().saturating_sub(1)).unwrap()
}

fn copy_file_range_all<InFd: AsFd, OutFd: AsFd>(
    fd_in: InFd,
    mut off_in: Option<&mut u64>,
    fd_out: OutFd,
    mut off_out: Option<&mut u64>,
    len: usize,
) -> rustix::io::Result<usize> {
    let mut total_copied = 0;
    loop {
        let byte_copied = copy_file_range(
            &fd_in,
            off_in.as_deref_mut(),
            &fd_out,
            off_out.as_deref_mut(),
            len - total_copied,
        )?;

        if byte_copied == 0 {
            break;
        }
        total_copied += byte_copied;
    }
    Ok(total_copied)
}

struct DirectFileNameToken<'a, T>(&'a mut [u8], PhantomData<T>);

impl<T> Deref for DirectFileNameToken<'_, T> {
    type Target = CStr;

    fn deref(&self) -> &Self::Target {
        unsafe { CStr::from_ptr(self.0.as_ptr().cast()) }
    }
}

fn direct_metadata_file_name(
    buf: &mut [u8; "256".len() + 1 + "4294967296".len() + ".metadata".len() + 1],
    to: RingKind,
    id: u32,
) -> DirectFileNameToken<()> {
    write!(buf.as_mut_slice(), "{:0>3}_{id:0>10}.metadata", to as u8).unwrap();
    DirectFileNameToken(buf.as_mut_slice(), PhantomData)
}

fn direct_file_name(buf: DirectFileNameToken<()>) -> DirectFileNameToken<Infallible> {
    buf.0[14] = 0;
    DirectFileNameToken(buf.0, PhantomData)
}

pub fn decompose_id(id: u64) -> Result<(RingKind, u32), IdNotFoundError> {
    match id >> 32 {
        0 => Ok(RingKind::Favorites),
        1 => Ok(RingKind::Main),
        ring => Err(IdNotFoundError::Ring(u32::try_from(ring).unwrap())),
    }
    .map(|ring| (ring, u32::try_from(id & u64::from(u32::MAX)).unwrap()))
}
