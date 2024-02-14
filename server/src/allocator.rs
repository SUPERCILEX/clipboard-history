use std::{
    cmp::max,
    fmt::Debug,
    fs,
    fs::File,
    io,
    io::{ErrorKind, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::FileExt,
    },
    path::{Path, PathBuf},
};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use clipboard_history_core::{
    ring::{BucketEntry, Entry, Ring, RingWriter},
    IoErr,
};
use rustix::fs::{copy_file_range, openat, statx, AtFlags, Mode, OFlags, StatxFlags, CWD};

use crate::{
    utils::TEXT_MIMES,
    views::{PathView, StringView},
    CliError,
};

pub struct Allocator {
    main_writer: RingWriter,
    main_ring: Ring,
    buckets: Buckets,
    direct_dir: OwnedFd,
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
            let lists =
                bitcode::decode(&bytes).map_err(|e| CliError::FreeListsDeserializeError {
                    file: path.into(),
                    error: e,
                })?;
            file.set_len(0)
                .map_io_err(|| format!("Failed to truncate free lists: {path:?}"))?;
            Ok(Self { lists, file })
        }
    }

    fn save(&mut self) -> Result<(), CliError> {
        let bytes = bitcode::encode(&self.lists).map_err(CliError::FreeListsSerializeError)?;
        self.file
            .write_all_at(&bytes, 0)
            .map_io_err(|| "Failed to write free lists.")?;
        Ok(())
    }

    fn alloc(&mut self, bucket: usize) -> Option<u32> {
        self.lists.0[bucket].pop()
    }

    fn free(&mut self, bucket: usize, index: u32) {
        self.lists.0[bucket].push(index);
    }
}

impl Allocator {
    pub fn open(mut data_dir: PathBuf) -> Result<Self, CliError> {
        let mut open_ring = |name| -> Result<_, CliError> {
            let main = PathView::new(&mut data_dir, name);
            Ok((
                RingWriter::open(&*main)?,
                Ring::open(10 /* todo */, &*main)?,
            ))
        };
        let (main_writer, main_ring) = open_ring("main.ring")?;

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
            main_writer,
            main_ring,
            buckets: Buckets {
                buckets,
                slot_counts,
                free_lists,
            },
            direct_dir,
        })
    }

    pub fn add(&mut self, data: OwnedFd) -> Result<u32, CliError> {
        let head = self.main_ring.write_head();

        let entry = self.alloc(data, "")?;
        self.deallocate(head)?;
        self.main_writer.write(entry, head)?;
        self.main_writer
            .set_write_head(if head == self.main_ring.capacity() - 1 {
                0
            } else {
                head + 1
            })?;

        Ok(head)
    }

    fn alloc(&mut self, data: OwnedFd, mime_type: &str) -> Result<Entry, CliError> {
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

        if TEXT_MIMES.contains(&mime_type) {
            let size = statx(&received, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                .map_io_err(|| "Failed to statx received data file.")?
                .stx_size;

            if size > 0 && size < 4096 {
                self.alloc_bucket(received, u32::try_from(size).unwrap())
            } else {
                self.alloc_direct(received, mime_type)
            }
        } else {
            self.alloc_direct(received, mime_type)
        }
    }

    fn alloc_bucket(&mut self, data: File, size: u32) -> Result<Entry, CliError> {
        let bucket = size_to_bucket(size);
        let Buckets {
            buckets,
            slot_counts: bucket_lengths,
            free_lists,
        } = &mut self.buckets;

        let free_bucket = free_lists.alloc(bucket);
        let bucket_index = free_bucket.unwrap_or_else(|| bucket_lengths[bucket]);
        let bucket_len = 1 << (bucket + 2);

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

        Ok(Entry::Bucketed(
            BucketEntry::new(size, bucket_index).unwrap(),
        ))
    }

    fn alloc_direct(&mut self, data: File, mime_type: &str) -> Result<Entry, CliError> {
        // TODO
        Ok(Entry::File)
    }

    fn deallocate(&mut self, id: u32) -> Result<(), CliError> {
        // TODO
        Ok(())
    }

    pub fn shutdown(mut self) -> Result<(), CliError> {
        self.buckets.free_lists.save()
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
