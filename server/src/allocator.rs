use std::{
    array,
    cmp::max,
    fmt::Debug,
    fs,
    fs::File,
    io::{ErrorKind, Read, Write},
    os::{fd::OwnedFd, unix::fs::FileExt},
    path::{Path, PathBuf},
};

use arrayvec::ArrayVec;
use bitcode::{Decode, Encode};
use clipboard_history_core::{
    ring::{BucketEntry, Entry, Ring, RingWriter},
    IoErr,
};
use rustix::fs::{
    copy_file_range, openat, statx, AtFlags, FileType, Mode, OFlags, StatxFlags, CWD,
};

use crate::{
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
    bucket_lengths: [u32; 11],
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
        let bucket_lengths = {
            let mut lengths = ArrayVec::new_const();

            for (i, bucket) in buckets.iter().enumerate() {
                lengths.push(
                    u32::try_from(
                        statx(bucket, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                            .map_io_err(|| "Failed to statx bucket.")?
                            .stx_size
                            >> (i + 2),
                    )
                    .unwrap(),
                );
            }

            lengths.into_inner().unwrap()
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
                bucket_lengths,
                free_lists,
            },
            direct_dir,
        })
    }

    pub fn add(&mut self, data: OwnedFd) -> Result<u32, CliError> {
        let head = self.main_ring.write_head();

        let entry = self.alloc(data)?;
        self.deallocate(head)?;
        self.main_writer.write(entry, head)?;
        self.main_writer.set_write_head(
            if usize::try_from(head).unwrap() == self.main_ring.capacity() - 1 {
                0
            } else {
                head + 1
            },
        )?;

        Ok(head)
    }

    fn alloc(&mut self, data: OwnedFd) -> Result<Entry, CliError> {
        // TODO rework this: always allocate temp file, write fd into there, then use
        //  copy_file_range's offset to determine file size. Cuz clipboard entries are
        //  almost always going to be pipes so mucking around with regular files is
        //  mostly pointless.

        let metadata = statx(
            &data,
            c"",
            AtFlags::EMPTY_PATH,
            StatxFlags::MODE | StatxFlags::SIZE,
        )
        .map_io_err(|| "Failed to statx new data.")?;

        match FileType::from_raw_mode(metadata.stx_mode.into()) {
            FileType::RegularFile if metadata.stx_size > 0 && metadata.stx_size < 4096 => {
                let size = u32::try_from(metadata.stx_size).unwrap();
                let bucket = size_to_bucket(size);
                let Buckets {
                    buckets,
                    bucket_lengths,
                    free_lists,
                } = &mut self.buckets;

                let bucket_index = free_lists
                    .alloc(bucket)
                    .unwrap_or_else(|| bucket_lengths[bucket]);
                let mut offset = u64::from(bucket_index) << (bucket + 2);
                let bucket_len = 1 << (bucket + 2);

                {
                    let mut total_copied = 0;
                    loop {
                        let byte_copied = copy_file_range(
                            &data,
                            None,
                            &buckets[bucket],
                            Some(&mut offset),
                            bucket_len - total_copied,
                        )
                        .map_io_err(|| format!("Failed to copy new data to bucket {bucket}.",))?;

                        if byte_copied == 0 {
                            break;
                        }
                        total_copied += byte_copied;
                    }
                }

                {
                    let grow = free_lists.alloc(bucket).is_none();
                    if grow {
                        bucket_lengths[bucket] += 1;
                    }

                    if usize::try_from(size).unwrap() < bucket_len {
                        buckets[bucket]
                            .write_all_at(
                                &[0],
                                if grow {
                                    (u64::from(bucket_lengths[bucket]) << (bucket + 2)) - 1
                                } else {
                                    offset
                                },
                            )
                            .map_io_err(|| {
                                format!("Failed to write NUL bytes to bucket {bucket}.",)
                            })?;
                    }
                }

                Ok(Entry::Bucketed(
                    BucketEntry::new(size, bucket_index).unwrap(),
                ))
            }
            _ => {
                // TODO write, do copy file range to temp file and then either copy to buckets
                // or
                Ok(Entry::File)
            }
        }
    }

    fn deallocate(&mut self, id: u32) -> Result<(), CliError> {
        todo!()
    }

    pub fn shutdown(mut self) -> Result<(), CliError> {
        self.buckets.free_lists.save()
    }
}

fn size_to_bucket(size: u32) -> usize {
    usize::try_from(max(1, size.saturating_sub(1)).ilog2().saturating_sub(1)).unwrap()
}
