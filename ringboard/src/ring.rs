use std::{
    fmt::Debug,
    fs::File,
    io::{ErrorKind, IoSlice, Write},
    mem,
    os::{fd::AsFd, unix::fs::FileExt},
    ptr,
    ptr::NonNull,
    slice,
};

use rustix::{
    fs::{openat, statx, AtFlags, Mode, OFlags, StatxFlags, CWD},
    mm::{mmap, munmap, MapFlags, ProtFlags},
    path::Arg,
};

use crate::{Error, IoErr, Result};

pub const MAX_ENTRIES: u32 = (1 << 20) - 1;

pub struct Ring {
    mem: Mmap,
    len: u32,
    capacity: u32,
}

const MAGIC: [u8; 3] = [0x4D, 0x18, 0x32];
const VERSION: u8 = 0;

#[repr(C)]
struct Header {
    magic: [u8; 3],
    version: u8,
    write_head: u32,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            write_head: 0,
        }
    }
}

const _: () = assert!(mem::size_of::<Header>() == 8);

#[repr(transparent)]
struct RawEntry(u32);

impl From<Entry> for RawEntry {
    fn from(value: Entry) -> Self {
        match value {
            Entry::Uninitialized => Self(0),
            Entry::Bucketed(BucketEntry { size, index }) => Self((index << 12) | size),
            Entry::File => Self(1 << (u32::BITS - 1)),
        }
    }
}

impl From<RawEntry> for Entry {
    fn from(RawEntry(value): RawEntry) -> Self {
        if value == 0 {
            return Entry::Uninitialized;
        }

        let size = value & ((1 << 12) - 1);
        let index = value >> 12;

        if size == 0 {
            Entry::File
        } else {
            Entry::Bucketed(BucketEntry { size, index })
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Entry {
    Uninitialized,
    Bucketed(BucketEntry),
    File,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct BucketEntry {
    size: u32,
    index: u32,
}

impl BucketEntry {
    pub fn new(size: u32, index: u32) -> std::result::Result<Self, ()> {
        if size > 0 && size < (1 << 12) && index < (1 << 20) {
            Ok(Self { size, index })
        } else {
            Err(())
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn index(&self) -> u32 {
        self.index
    }
}

struct Mmap {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mmap {
    fn new<Fd: AsFd>(fd: Fd, len: usize) -> rustix::io::Result<Self> {
        Ok(Self {
            ptr: unsafe {
                NonNull::new_unchecked(mmap(
                    ptr::null_mut(),
                    len,
                    ProtFlags::READ,
                    MapFlags::SHARED_VALIDATE,
                    fd,
                    0,
                )?)
            }
            .cast(),
            len,
        })
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

impl Ring {
    /// Open a Ringboard database.
    #[allow(clippy::missing_panics_doc)]
    pub fn open<P: Arg + Copy + Debug>(max_entries: u32, path: P) -> Result<Self> {
        let max_entries = max_entries.clamp(1, MAX_ENTRIES);

        let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty())
            .map_io_err(|| format!("Failed to open Ringboard database for reading: {path:?}"))?;

        let len = statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
            .map_io_err(|| "Failed to statx Ringboard database file.")?
            .stx_size;
        let len = usize::try_from(len).unwrap();
        let mem = Mmap::new(fd, usize::try_from(entries_to_offset(max_entries)).unwrap())
            .map_io_err(|| "Failed to map memory.")?;

        if len < MAGIC.len()
            || unsafe { slice::from_raw_parts(mem.ptr.as_ptr(), MAGIC.len()) } != MAGIC
        {
            return Err(Error::NotARingboard {
                file: path.to_string_lossy().into_owned().into(),
            });
        }

        Ok(Self {
            mem,
            len: offset_to_entries(len),
            capacity: max_entries,
        })
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub unsafe fn set_len(&mut self, len: u32) {
        self.len = len;
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    pub fn write_head(&self) -> u32 {
        let bytes = unsafe {
            slice::from_raw_parts(
                self.mem
                    .ptr
                    .as_ptr()
                    .add(MAGIC.len() + mem::size_of_val(&VERSION)),
                mem::size_of::<u32>(),
            )
        };
        u32::from_le_bytes(bytes.try_into().unwrap())
    }

    pub fn next_head(&self, current: u32) -> u32 {
        if current == self.capacity() - 1 {
            0
        } else {
            current + 1
        }
    }

    pub fn get(&self, index: u32) -> Option<Entry> {
        if index >= self.len() {
            return None;
        }

        let bytes = unsafe {
            slice::from_raw_parts(
                self.mem
                    .ptr
                    .as_ptr()
                    .add(usize::try_from(entries_to_offset(index)).unwrap()),
                mem::size_of::<u32>(),
            )
        };
        let raw = RawEntry(u32::from_le_bytes(bytes.try_into().unwrap()));
        Some(Entry::from(raw))
    }
}

pub struct RingWriter {
    ring: File,
}

impl RingWriter {
    pub fn open<P: Arg + Copy + Debug>(path: P) -> Result<Self> {
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

    pub fn write(&mut self, entry: Entry, at: u32) -> Result<()> {
        self.ring
            .write_all_at(
                &RawEntry::from(entry).0.to_le_bytes(),
                entries_to_offset(at),
            )
            .map_io_err(|| format!("Failed to write entry to Ringboard database: {entry:?}"))
    }

    pub fn set_write_head(&mut self, head: u32) -> Result<()> {
        self.ring
            .write_all_at(
                &head.to_le_bytes(),
                u64::try_from(MAGIC.len() + mem::size_of_val(&VERSION)).unwrap(),
            )
            .map_io_err(|| format!("Failed to update Ringboard write head: {head}"))
    }
}

fn entries_to_offset(entries: u32) -> u64 {
    u64::from(entries) * u64::try_from(mem::size_of::<RawEntry>()).unwrap()
        + u64::try_from(mem::size_of::<Header>()).unwrap()
}

fn offset_to_entries(offset: usize) -> u32 {
    u32::try_from(offset.saturating_sub(mem::size_of::<Header>()) / mem::size_of::<RawEntry>())
        .unwrap()
}
