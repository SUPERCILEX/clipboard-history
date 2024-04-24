use std::{
    borrow::Cow,
    fs::File,
    io,
    io::{ErrorKind, Read},
    ops::{Deref, DerefMut},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::fs::FileExt,
    },
    path::PathBuf,
    slice, str,
};

use arrayvec::ArrayVec;
use ringboard_core::{
    bucket_to_length, direct_file_name, open_buckets,
    protocol::{MimeType, RingKind},
    ring::{BucketEntry, Mmap, Ring},
    size_to_bucket, IoErr, PathView,
};
use rustix::{
    fs::{fgetxattr, memfd_create, openat, MemfdFlags, Mode, OFlags, CWD},
    io::Errno,
};

#[derive(Debug)]
struct RingIter {
    kind: RingKind,

    front: u32,
    back: u32,
    done: bool,
}

impl RingIter {
    fn next(&mut self, ring: &Ring) -> Option<Entry> {
        self.next_dir(ring, |me| {
            let id = me.front;
            me.front = ring.next_entry(id);
            id
        })
    }

    fn next_back(&mut self, ring: &Ring) -> Option<Entry> {
        self.next_dir(ring, |me| {
            let id = me.back;
            me.back = ring.prev_entry(id);
            id
        })
    }

    fn next_dir(
        &mut self,
        ring: &Ring,
        mut advance: impl FnMut(&mut Self) -> u32,
    ) -> Option<Entry> {
        loop {
            use ringboard_core::ring::Entry::{Bucketed, File, Uninitialized};

            if self.done {
                return None;
            }
            self.done = self.front == self.back;

            let id = advance(self);
            let entry = Entry {
                id,
                ring: self.kind,
                kind: match ring.get(id)? {
                    Uninitialized => continue,
                    Bucketed(e) => Kind::Bucket(e),
                    File => Kind::File,
                },
            };

            break Some(entry);
        }
    }

    fn size_hint(&self, ring: &Ring) -> (usize, Option<usize>) {
        let len = if self.front > self.back {
            ring.len() - self.front + self.back
        } else {
            self.back - self.front
        };
        let len = usize::try_from(len).unwrap();
        (len, Some(len))
    }
}

#[derive(Debug)]
pub struct RingReader<'a> {
    ring: &'a Ring,
    iter: RingIter,
}

impl<'a> RingReader<'a> {
    #[must_use]
    pub fn from_ring(ring: &'a Ring, kind: RingKind) -> Self {
        let back = ring.prev_entry(ring.write_head());
        Self {
            iter: RingIter {
                kind,

                back,
                front: ring.next_entry(back),
                done: false,
            },
            ring,
        }
    }

    pub fn prepare_ring(
        database_dir: &mut PathBuf,
        kind: RingKind,
    ) -> Result<Ring, ringboard_core::Error> {
        let ring = PathView::new(
            database_dir,
            match kind {
                RingKind::Main => "main.ring",
                RingKind::Favorites => "favorites.ring",
            },
        );
        Ring::open(0, &*ring)
    }
}

impl Iterator for RingReader<'_> {
    type Item = Entry;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next(self.ring)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint(self.ring)
    }
}

impl DoubleEndedIterator for RingReader<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back(self.ring)
    }
}

#[derive(Debug)]
pub struct Entry {
    id: u32,
    ring: RingKind,
    kind: Kind,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Kind {
    Bucket(BucketEntry),
    File,
}

pub struct LoadedEntry<T> {
    loaded: T,
    fd: Option<LoadedEntryFd>,
}

enum LoadedEntryFd {
    Owned(OwnedFd),
    HackySelfReference(BorrowedFd<'static>),
}

impl<T> LoadedEntry<T> {
    pub fn into_inner(self) -> T {
        self.loaded
    }

    pub fn mime_type(&self) -> Result<MimeType, ringboard_core::Error> {
        let Some(fd) = self.backing_file() else {
            return Ok(MimeType::new());
        };

        let mut mime_type = [0u8; MimeType::new_const().capacity()];
        let len = match fgetxattr(fd, c"user.mime_type", &mut mime_type) {
            Err(Errno::NODATA) => {
                return Ok(MimeType::new());
            }
            r => r.map_io_err(|| "Failed to read extended attributes.")?,
        };
        let mime_type =
            str::from_utf8(&mime_type[..len]).map_err(|e| ringboard_core::Error::Io {
                error: io::Error::new(ErrorKind::InvalidInput, e),
                context: "Database corruption detected: invalid mime type detected".into(),
            })?;

        Ok(MimeType::from(mime_type).unwrap())
    }

    pub fn backing_file(&self) -> Option<BorrowedFd> {
        self.fd.as_ref().map(|fd| match fd {
            LoadedEntryFd::Owned(o) => o.as_fd(),
            LoadedEntryFd::HackySelfReference(b) => *b,
        })
    }
}

impl<T> Deref for LoadedEntry<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.loaded
    }
}

impl<T> DerefMut for LoadedEntry<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.loaded
    }
}

impl Entry {
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    pub fn to_slice<'a>(
        &self,
        reader: &'a EntryReader,
    ) -> Result<LoadedEntry<Cow<'a, [u8]>>, ringboard_core::Error> {
        match self.kind {
            Kind::Bucket(entry) => Ok(LoadedEntry {
                loaded: bucket_entry_to_slice(reader, entry).unwrap().into(),
                fd: None,
            }),
            Kind::File => {
                let mut v = Vec::new();
                let mut file = self.to_file(reader)?;
                file.read_to_end(&mut v).map_io_err(|| {
                    format!(
                        "Failed to read direct entry {} in {:?} ring",
                        self.id, self.ring
                    )
                })?;
                Ok(LoadedEntry {
                    loaded: v.into(),
                    fd: Some(LoadedEntryFd::Owned(file.loaded.into())),
                })
            }
        }
    }

    pub fn to_file(
        &self,
        reader: &EntryReader,
    ) -> Result<LoadedEntry<File>, ringboard_core::Error> {
        match self.kind {
            Kind::Bucket(entry) => {
                // TODO handle this (and above) unwrap properly: we need to grow the EntryReader
                //  mappings.
                let bytes = bucket_entry_to_slice(reader, entry).unwrap();
                let file = File::from(
                    memfd_create("ringboard_bucket_reader", MemfdFlags::empty())
                        .map_io_err(|| "Failed to create data entry file.")?,
                );

                file.write_all_at(bytes, 0)
                    .map_io_err(|| "Failed to write bytes to entry file.")?;

                Ok(LoadedEntry {
                    loaded: file,
                    fd: None,
                })
            }
            Kind::File => {
                let mut buf = Default::default();
                let buf = direct_file_name(&mut buf, self.ring, self.id);

                let file = openat(&reader.direct, &*buf, OFlags::RDONLY, Mode::empty())
                    .map_io_err(|| format!("Failed to open direct file: {buf:?}"))
                    .map(File::from)?;
                Ok(LoadedEntry {
                    fd: Some(LoadedEntryFd::HackySelfReference(unsafe {
                        BorrowedFd::borrow_raw(file.as_raw_fd())
                    })),
                    loaded: file,
                })
            }
        }
    }
}

#[derive(Debug)]
pub struct EntryReader {
    buckets: [Mmap; 11],
    direct: OwnedFd,
}

impl EntryReader {
    pub fn open(database_dir: &mut PathBuf) -> Result<Self, ringboard_core::Error> {
        let buckets = {
            let mut buckets = PathView::new(database_dir, "buckets");
            let (buckets, lengths) = open_buckets(|name| {
                let file = PathView::new(&mut buckets, name);
                openat(CWD, &*file, OFlags::RDONLY, Mode::empty())
                    .map_io_err(|| format!("Failed to open bucket: {file:?}"))
            })?;

            let mut maps = ArrayVec::new_const();
            for (i, fd) in buckets.into_iter().enumerate() {
                maps.push(
                    Mmap::new(fd, usize::try_from(lengths[i]).unwrap().max(4096))
                        .map_io_err(|| "Failed to map memory.")?,
                );
            }
            maps.into_inner().unwrap()
        };

        let direct_dir = {
            let file = PathView::new(database_dir, "direct");
            openat(CWD, &*file, OFlags::DIRECTORY | OFlags::PATH, Mode::empty())
                .map_io_err(|| format!("Failed to open directory: {file:?}"))
        }?;

        Ok(Self {
            buckets,
            direct: direct_dir,
        })
    }

    #[must_use]
    pub fn buckets(&self) -> [&Mmap; 11] {
        let mut buckets = ArrayVec::new_const();
        for bucket in &self.buckets {
            buckets.push(bucket);
        }
        buckets.into_inner().unwrap()
    }
}

fn bucket_entry_to_slice(reader: &EntryReader, entry: BucketEntry) -> Option<&[u8]> {
    let index = usize::try_from(entry.index()).unwrap();
    let size = usize::try_from(entry.size()).unwrap();
    let bucket = size_to_bucket(entry.size());

    let start = usize::try_from(bucket_to_length(bucket)).unwrap() * index;
    let mem = &reader.buckets[bucket];
    if start + size > mem.len() {
        return None;
    }

    let ptr = mem.ptr().as_ptr();
    Some(unsafe { slice::from_raw_parts(ptr.add(start), size) })
}
