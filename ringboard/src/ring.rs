use std::{fmt::Debug, mem, ops::Deref, os::fd::AsFd, ptr, ptr::NonNull, slice};

use rustix::{
    fs::{openat, statx, AtFlags, Mode, OFlags, StatxFlags, CWD},
    mm::{mmap, mremap, munmap, MapFlags, MremapFlags, ProtFlags},
    path::Arg,
};

use crate::{Error, IoErr, Result};

pub const MAX_ENTRIES: u32 = (1 << 20) - 1;

#[derive(Debug)]
pub struct Ring {
    mem: Mmap,
    len: u32,
    capacity: u32,
}

pub const MAGIC: [u8; 3] = [0x4D, 0x18, 0x32];
pub const VERSION: u8 = 0;

#[repr(C)]
pub struct Header {
    pub magic: [u8; 3],
    pub version: u8,
    pub write_head: u32,
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
pub struct RawEntry(u32);

impl Deref for RawEntry {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

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
            return Self::Uninitialized;
        }

        let size = value & ((1 << 12) - 1);
        let index = value >> 12;

        if size == 0 {
            Self::File
        } else {
            Self::Bucketed(BucketEntry { size, index })
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
    #[must_use]
    pub const fn new(size: u32, index: u32) -> Option<Self> {
        if size > 0 && size < (1 << 12) && index < (1 << 20) {
            Some(Self { size, index })
        } else {
            None
        }
    }

    #[must_use]
    pub const fn size(&self) -> u32 {
        self.size
    }

    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
}

#[derive(Debug)]
pub struct Mmap {
    ptr: NonNull<u8>,
    len: usize,
}

unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}

impl Mmap {
    pub fn from<Fd: AsFd>(fd: Fd) -> rustix::io::Result<Self> {
        let len = statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)?.stx_size;
        Self::new(fd, usize::try_from(len).unwrap())
    }

    pub fn new<Fd: AsFd>(fd: Fd, len: usize) -> rustix::io::Result<Self> {
        if len == 0 {
            return Ok(Self {
                ptr: NonNull::dangling(),
                len,
            });
        }

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

    pub fn remap(&mut self, len: usize) -> rustix::io::Result<()> {
        self.ptr = unsafe {
            NonNull::new_unchecked(
                mremap(
                    self.ptr.as_ptr().cast(),
                    self.len,
                    len,
                    MremapFlags::MAYMOVE,
                )?
                .cast(),
            )
        };
        self.len = len;
        Ok(())
    }

    #[must_use]
    pub const fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for Mmap {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr().as_ptr(), self.len()) }
    }
}

impl AsRef<[u8]> for Mmap {
    fn as_ref(&self) -> &[u8] {
        self.deref()
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
        let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty())
            .map_io_err(|| format!("Failed to open Ringboard database for reading: {path:?}"))?;

        let len = statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
            .map_io_err(|| "Failed to statx Ringboard database file.")?
            .stx_size;
        let len = usize::try_from(len).unwrap();
        let max_entries = max_entries.clamp(offset_to_entries(len), MAX_ENTRIES);
        let mem = Mmap::new(fd, usize::try_from(entries_to_offset(max_entries)).unwrap())
            .map_io_err(|| "Failed to map memory.")?;

        if len < MAGIC.len()
            || unsafe { slice::from_raw_parts(mem.ptr().as_ptr(), MAGIC.len()) } != MAGIC
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

    #[must_use]
    pub const fn len(&self) -> u32 {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// # Safety
    ///
    /// The ring file must have at least len entries and cannot exceed capacity.
    pub unsafe fn set_len(&mut self, len: u32) {
        debug_assert!(len <= self.capacity());
        self.len = len;
    }

    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    #[must_use]
    #[allow(clippy::missing_panics_doc)]
    pub fn write_head(&self) -> u32 {
        let bytes = unsafe {
            slice::from_raw_parts(
                self.mem
                    .ptr()
                    .as_ptr()
                    .add(MAGIC.len() + mem::size_of_val(&VERSION)),
                mem::size_of::<u32>(),
            )
        };
        u32::from_le_bytes(bytes.try_into().unwrap())
    }

    #[must_use]
    pub const fn next_head(&self, current: u32) -> u32 {
        if current == self.capacity() - 1 {
            0
        } else {
            current + 1
        }
    }

    #[must_use]
    pub const fn next_entry(&self, current: u32) -> u32 {
        if self.is_empty() {
            return current;
        }

        if current == self.len() - 1 {
            0
        } else {
            current + 1
        }
    }

    #[must_use]
    pub const fn prev_entry(&self, current: u32) -> u32 {
        if self.is_empty() {
            return current;
        }

        if current == 0 {
            self.len() - 1
        } else {
            current - 1
        }
    }

    #[must_use]
    #[allow(clippy::missing_panics_doc)]
    pub fn get(&self, index: u32) -> Option<Entry> {
        if index >= self.len() {
            return None;
        }

        let bytes = unsafe {
            slice::from_raw_parts(
                self.mem
                    .ptr()
                    .as_ptr()
                    .add(usize::try_from(entries_to_offset(index)).unwrap()),
                mem::size_of::<u32>(),
            )
        };
        let raw = RawEntry(u32::from_le_bytes(bytes.try_into().unwrap()));
        Some(Entry::from(raw))
    }
}

#[must_use]
pub fn entries_to_offset(entries: u32) -> u64 {
    u64::from(entries) * u64::try_from(mem::size_of::<RawEntry>()).unwrap()
        + u64::try_from(mem::size_of::<Header>()).unwrap()
}

#[must_use]
pub fn offset_to_entries(offset: usize) -> u32 {
    u32::try_from(offset.saturating_sub(mem::size_of::<Header>()) / mem::size_of::<RawEntry>())
        .unwrap()
}
