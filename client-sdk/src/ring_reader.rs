use std::{
    fmt::{Debug, Formatter},
    fs::File,
    io,
    io::ErrorKind,
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
    protocol::{composite_id, decompose_id, IdNotFoundError, MimeType, RingKind},
    ring::{BucketEntry, Mmap, Ring},
    size_to_bucket, IoErr, PathView, NUM_BUCKETS,
};
use rustix::{
    fs::{fgetxattr, memfd_create, openat, MemfdFlags, Mode, OFlags, CWD},
    io::Errno,
};

#[derive(Debug)]
struct RingIter {
    kind: RingKind,

    write_head: u32,
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
            if self.done {
                return None;
            }
            self.done = self.front == self.back
                || ring.next_head(self.front) == self.write_head
                || self.back == self.write_head;

            if let Some(entry) = Entry::from(ring, self.kind, advance(self)) {
                break Some(entry);
            }
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
pub struct DatabaseReader {
    main: Ring,
    favorites: Ring,
}

impl DatabaseReader {
    pub fn open(database: &mut PathBuf) -> Result<Self, ringboard_core::Error> {
        Ok(Self {
            main: RingReader::prepare_ring(database, RingKind::Main)?,
            favorites: RingReader::prepare_ring(database, RingKind::Favorites)?,
        })
    }

    pub fn get_raw(&self, id: u64) -> Result<Entry, IdNotFoundError> {
        let (kind, id) = decompose_id(id)?;
        Entry::from(
            match kind {
                RingKind::Favorites => &self.favorites,
                RingKind::Main => &self.main,
            },
            kind,
            id,
        )
        .ok_or(IdNotFoundError::Entry(id))
    }

    /// # Safety
    ///
    /// The ID must index into a ring whose length is greater than the index
    /// contained within the ID.
    pub unsafe fn get(&mut self, id: u64) -> Result<Entry, IdNotFoundError> {
        let (kind, sub_id) = decompose_id(id)?;
        let ring = match kind {
            RingKind::Favorites => &mut self.favorites,
            RingKind::Main => &mut self.main,
        };
        if sub_id >= ring.len() {
            unsafe {
                ring.set_len(sub_id + 1);
            }
        }
        self.get_raw(id)
    }

    pub fn main_ring_mut(&mut self) -> &mut Ring {
        &mut self.main
    }

    pub fn favorites_ring_mut(&mut self) -> &mut Ring {
        &mut self.favorites
    }

    #[must_use]
    pub fn main(&self) -> RingReader {
        RingReader::from_ring(&self.main, RingKind::Main)
    }

    #[must_use]
    pub fn favorites(&self) -> RingReader {
        RingReader::from_ring(&self.favorites, RingKind::Favorites)
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
        let tail = ring.write_head();
        Self::from_id(ring, kind, tail, tail)
    }

    #[must_use]
    pub fn from_id(ring: &'a Ring, kind: RingKind, write_head: u32, id: u32) -> Self {
        let mut me = Self::from_uninit(ring, kind);
        me.reset_to(write_head, id);
        me
    }

    #[must_use]
    pub const fn from_uninit(ring: &'a Ring, kind: RingKind) -> Self {
        Self {
            iter: RingIter {
                kind,

                write_head: 0,
                back: 0,
                front: 0,
                done: true,
            },
            ring,
        }
    }

    pub fn prepare_ring(
        database_dir: &mut PathBuf,
        kind: RingKind,
    ) -> Result<Ring, ringboard_core::Error> {
        let ring = PathView::new(database_dir, kind.file_name());
        Ring::open(kind.default_max_entries(), &*ring)
    }

    #[must_use]
    pub const fn ring(&self) -> &Ring {
        self.ring
    }

    #[must_use]
    pub const fn kind(&self) -> RingKind {
        self.iter.kind
    }

    pub fn reset_to(&mut self, write_head: u32, start: u32) {
        let RingIter {
            kind: _,
            write_head: old_write_head,
            back,
            front,
            done,
        } = &mut self.iter;

        *old_write_head = write_head;
        *back = self.ring.prev_entry(start);
        *front = self.ring.next_entry(*back);
        *done = false;
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

#[derive(Copy, Clone, Debug)]
pub struct Entry {
    id: u32,
    ring: RingKind,
    kind: Kind,
}

impl Entry {
    fn from(ring: &Ring, kind: RingKind, id: u32) -> Option<Self> {
        use ringboard_core::ring::Entry::{Bucketed, File, Uninitialized};
        Some(Self {
            id,
            ring: kind,
            kind: match ring.get(id)? {
                Uninitialized => return None,
                Bucketed(e) => Kind::Bucket(e),
                File => Kind::File,
            },
        })
    }
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

impl<T: Debug> Debug for LoadedEntry<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.loaded.fmt(f)
    }
}

enum LoadedEntryFd {
    Owned(OwnedFd),
    HackySelfReference(BorrowedFd<'static>),
}

pub fn xattr_mime_type<Fd: AsFd>(fd: Fd) -> Result<MimeType, ringboard_core::Error> {
    let mut mime_type = [0u8; MimeType::new_const().capacity()];
    let len = match fgetxattr(fd, c"user.mime_type", &mut mime_type) {
        Err(Errno::NODATA) => {
            return Ok(MimeType::new());
        }
        r => r.map_io_err(|| "Failed to read extended attributes.")?,
    };
    let mime_type = str::from_utf8(&mime_type[..len]).map_err(|e| ringboard_core::Error::Io {
        error: io::Error::new(ErrorKind::InvalidInput, e),
        context: "Database corruption detected: invalid mime type detected".into(),
    })?;

    Ok(MimeType::from(mime_type).unwrap())
}

impl<T> LoadedEntry<T> {
    pub fn into_inner(self) -> T {
        self.loaded
    }

    pub fn mime_type(&self) -> Result<MimeType, ringboard_core::Error> {
        let Some(fd) = self.backing_file() else {
            return Ok(MimeType::new());
        };

        xattr_mime_type(fd)
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

pub enum MmapOrSlice<'a> {
    Slice(&'a [u8]),
    Mmap(Mmap),
}

impl<'a> From<&'a [u8]> for MmapOrSlice<'a> {
    fn from(value: &'a [u8]) -> Self {
        Self::Slice(value)
    }
}

impl From<Mmap> for MmapOrSlice<'_> {
    fn from(value: Mmap) -> Self {
        Self::Mmap(value)
    }
}

impl<'a> Deref for MmapOrSlice<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            MmapOrSlice::Slice(s) => s,
            MmapOrSlice::Mmap(m) => m,
        }
    }
}

impl Entry {
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    #[must_use]
    pub const fn ring(&self) -> RingKind {
        self.ring
    }

    #[must_use]
    pub const fn index(&self) -> u32 {
        self.id
    }

    #[must_use]
    pub fn id(&self) -> u64 {
        composite_id(self.ring, self.id)
    }

    pub fn mime_type(&self, reader: &mut EntryReader) -> Result<MimeType, ringboard_core::Error> {
        match self.kind() {
            Kind::Bucket(_) => Ok(MimeType::new()),
            Kind::File => self.to_file(reader)?.mime_type(),
        }
    }

    pub fn to_slice<'a>(
        &self,
        reader: &'a mut EntryReader,
    ) -> Result<LoadedEntry<MmapOrSlice<'a>>, ringboard_core::Error> {
        self.grow_bucket_if_needed(reader)?;
        Ok(self.to_slice_raw(reader)?.unwrap())
    }

    pub fn to_file(
        &self,
        reader: &mut EntryReader,
    ) -> Result<LoadedEntry<File>, ringboard_core::Error> {
        self.grow_bucket_if_needed(reader)?;
        Ok(self.to_file_raw(reader)?.unwrap())
    }

    fn grow_bucket_if_needed(&self, reader: &mut EntryReader) -> Result<(), ringboard_core::Error> {
        match self.kind {
            Kind::Bucket(entry) => {
                if let Err(BucketTooShort { bucket, needed_len }) =
                    bucket_entry_to_slice(reader, entry)
                {
                    let bucket = &mut reader.buckets[bucket];
                    bucket
                        .remap(needed_len.max(bucket.len() * 2))
                        .map_io_err(|| "Failed to remap bucket.")?;
                }
            }
            Kind::File => {}
        }
        Ok(())
    }

    pub fn to_slice_raw<'a>(
        &self,
        reader: &'a EntryReader,
    ) -> Result<Option<LoadedEntry<MmapOrSlice<'a>>>, ringboard_core::Error> {
        match self.kind {
            Kind::Bucket(entry) => {
                let Ok(bytes) = bucket_entry_to_slice(reader, entry) else {
                    return Ok(None);
                };
                Ok(Some(LoadedEntry {
                    loaded: bytes.into(),
                    fd: None,
                }))
            }
            Kind::File => {
                let Some(file) = self.to_file_raw(reader)? else {
                    return Ok(None);
                };
                Ok(Some(LoadedEntry {
                    loaded: Mmap::from(&*file)
                        .map_io_err(|| format!("Failed to mmap data file: {file:?}"))?
                        .into(),
                    fd: Some(LoadedEntryFd::Owned(file.loaded.into())),
                }))
            }
        }
    }

    pub fn to_file_raw(
        &self,
        reader: &EntryReader,
    ) -> Result<Option<LoadedEntry<File>>, ringboard_core::Error> {
        match self.kind {
            Kind::Bucket(entry) => {
                let Ok(bytes) = bucket_entry_to_slice(reader, entry) else {
                    return Ok(None);
                };
                let file = File::from(
                    memfd_create(c"ringboard_bucket_reader", MemfdFlags::empty())
                        .map_io_err(|| "Failed to create data entry file.")?,
                );

                file.write_all_at(bytes, 0)
                    .map_io_err(|| "Failed to write bytes to entry file.")?;

                Ok(Some(LoadedEntry {
                    loaded: file,
                    fd: None,
                }))
            }
            Kind::File => {
                let mut buf = Default::default();
                let buf = direct_file_name(&mut buf, self.ring, self.id);

                let file = openat(&reader.direct, &*buf, OFlags::RDONLY, Mode::empty())
                    .map_io_err(|| format!("Failed to open direct file: {buf:?}"))
                    .map(File::from)?;
                Ok(Some(LoadedEntry {
                    fd: Some(LoadedEntryFd::HackySelfReference(unsafe {
                        BorrowedFd::borrow_raw(file.as_raw_fd())
                    })),
                    loaded: file,
                }))
            }
        }
    }
}

#[derive(Debug)]
pub struct EntryReader {
    buckets: [Mmap; NUM_BUCKETS],
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
                    Mmap::new(fd, usize::try_from(lengths[i]).unwrap())
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
    pub fn buckets(&self) -> [&Mmap; NUM_BUCKETS] {
        let mut buckets = ArrayVec::new_const();
        for bucket in &self.buckets {
            buckets.push(bucket);
        }
        buckets.into_inner().unwrap()
    }

    #[must_use]
    pub fn direct(&self) -> BorrowedFd {
        self.direct.as_fd()
    }
}

struct BucketTooShort {
    bucket: usize,
    needed_len: usize,
}

fn bucket_entry_to_slice(
    reader: &EntryReader,
    entry: BucketEntry,
) -> Result<&[u8], BucketTooShort> {
    let index = usize::try_from(entry.index()).unwrap();
    let size = usize::from(entry.size());
    let bucket = usize::from(size_to_bucket(entry.size()));

    let size_class = usize::from(bucket_to_length(bucket));
    let start = size_class * index;
    let mem = &reader.buckets[bucket];
    if start + size > mem.len() {
        return Err(BucketTooShort {
            bucket,
            needed_len: size_class * (index + 1),
        });
    }

    let ptr = mem.ptr().as_ptr();
    Ok(unsafe { slice::from_raw_parts(ptr.add(start), size) })
}
