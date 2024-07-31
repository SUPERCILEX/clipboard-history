use std::{
    fmt::{Debug, Formatter},
    fs, io,
    io::ErrorKind,
    ops::Deref,
    os::fd::{AsFd, AsRawFd},
    path::PathBuf,
    ptr,
    ptr::NonNull,
    slice,
};

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
    #[cfg(debug_assertions)]
    fd: std::os::fd::OwnedFd,
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

const _: () = assert!(size_of::<Header>() == 8);

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
            Entry::Bucketed(InitializedEntry(data)) => Self(data),
            Entry::File => Self(InitializedEntry::file().0),
        }
    }
}

#[allow(clippy::fallible_impl_from)]
impl From<RawEntry> for Entry {
    fn from(RawEntry(value): RawEntry) -> Self {
        if value == 0 {
            return Self::Uninitialized;
        }

        let entry = InitializedEntry(value);
        if entry.is_file() {
            Self::File
        } else {
            Self::Bucketed(entry)
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum Entry {
    Uninitialized,
    Bucketed(InitializedEntry),
    File,
}

#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct InitializedEntry(u32);

impl InitializedEntry {
    #[must_use]
    pub fn bucket(size: u16, index: u32) -> Self {
        debug_assert!(size > 0);
        debug_assert!(size < (1 << 12));
        debug_assert!(index < MAX_ENTRIES);
        Self((index << 12) | u32::from(size))
    }

    #[must_use]
    pub const fn file() -> Self {
        Self(1 << (u32::BITS - 1))
    }

    #[must_use]
    pub fn size(&self) -> u16 {
        u16::try_from(self.0 & ((1 << 12) - 1)).unwrap()
    }

    #[must_use]
    pub const fn index(&self) -> u32 {
        self.0 >> 12
    }

    #[must_use]
    pub fn is_file(&self) -> bool {
        self.size() == 0
    }
}

impl Debug for InitializedEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.is_file() {
            f.write_str("File")
        } else {
            f.debug_struct("Bucketed")
                .field("size", &self.size())
                .field("index", &self.index())
                .finish()
        }
    }
}

#[derive(Debug)]
pub struct Mmap {
    ptr: NonNull<u8>,
    requested_len: usize,
    backing_len: usize,
}

unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}

impl Mmap {
    pub fn from<Fd: AsFd>(fd: Fd) -> rustix::io::Result<Self> {
        let len = statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)?.stx_size;
        Self::new(fd, usize::try_from(len).unwrap())
    }

    pub fn new<Fd: AsFd>(fd: Fd, len: usize) -> rustix::io::Result<Self> {
        let backing_len = len.max(4096);
        Ok(Self {
            ptr: unsafe {
                NonNull::new_unchecked(mmap(
                    ptr::null_mut(),
                    backing_len,
                    ProtFlags::READ,
                    MapFlags::SHARED_VALIDATE,
                    fd,
                    0,
                )?)
            }
            .cast(),
            requested_len: len,
            backing_len,
        })
    }

    pub fn remap(&mut self, len: usize) -> rustix::io::Result<()> {
        if len >= self.requested_len && len <= self.backing_len {
            self.requested_len = len;
            return Ok(());
        }

        let backing_len = len.max(
            self.backing_len
                .saturating_add(1)
                .checked_next_power_of_two()
                .unwrap_or(usize::MAX),
        );
        self.ptr = unsafe {
            NonNull::new_unchecked(
                mremap(
                    self.ptr.as_ptr().cast(),
                    self.backing_len,
                    backing_len,
                    MremapFlags::MAYMOVE,
                )?
                .cast(),
            )
        };
        self.requested_len = len;
        self.backing_len = backing_len;
        Ok(())
    }

    #[must_use]
    pub const fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.requested_len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.requested_len == 0
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
        self
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.ptr.as_ptr().cast(), self.backing_len) };
    }
}

impl Ring {
    /// Open a Ringboard database.
    pub fn open<P: Arg + Copy + Debug>(max_entries: u32, path: P) -> Result<Self> {
        let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty())
            .map_io_err(|| format!("Failed to open Ringboard database for reading: {path:?}"))?;
        Self::open_fd(max_entries, fd)
    }

    pub fn open_fd<Fd: AsFd>(max_entries: u32, fd: Fd) -> Result<Self> {
        let len = statx(&fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
            .map_io_err(|| "Failed to statx Ringboard database file.")?
            .stx_size;
        let len = usize::try_from(len).unwrap();
        let max_entries = max_entries.clamp(offset_to_entries(len), MAX_ENTRIES);
        let mem = Mmap::new(
            &fd,
            usize::try_from(entries_to_offset(max_entries)).unwrap(),
        )
        .map_io_err(|| "Failed to mmap ring.")?;

        if len < MAGIC.len()
            || unsafe { slice::from_raw_parts(mem.ptr().as_ptr(), MAGIC.len()) } != MAGIC
        {
            let path = fs::read_link(PathBuf::from(format!(
                "/proc/self/fd/{}",
                fd.as_fd().as_raw_fd()
            )))
            .unwrap_or_else(|_| PathBuf::from("unknown"));
            return Err(Error::Io {
                error: io::Error::new(ErrorKind::InvalidData, "Not a Ringboard database."),
                context: format!("Ring file has invalid magic header: {path:?}").into(),
            });
        }

        Ok(Self {
            mem,
            len: offset_to_entries(len),
            capacity: max_entries,
            #[cfg(debug_assertions)]
            fd: fd
                .as_fd()
                .try_clone_to_owned()
                .map_io_err(|| "Failed to clone ring FD.")?,
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
        #[cfg(debug_assertions)]
        {
            let bytes = statx(&self.fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                .map_io_err(|| "Failed to statx Ringboard database file.")
                .unwrap()
                .stx_size;
            let actual_len = offset_to_entries(usize::try_from(bytes).unwrap());
            debug_assert!(
                len <= actual_len,
                "Trying to resize ring of length {actual_len} to {len}."
            );
        }

        self.len = len;
    }

    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    #[must_use]
    pub fn write_head(&self) -> u32 {
        let bytes = unsafe {
            slice::from_raw_parts(
                self.mem
                    .ptr()
                    .as_ptr()
                    .add(MAGIC.len() + size_of_val(&VERSION)),
                size_of::<u32>(),
            )
        };
        u32::from_le_bytes(bytes.try_into().unwrap())
    }

    #[must_use]
    pub const fn next_head(&self, current: u32) -> u32 {
        if current >= self.capacity() - 1 {
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

        if current >= self.len() - 1 {
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
                size_of::<u32>(),
            )
        };
        let raw = RawEntry(u32::from_le_bytes(bytes.try_into().unwrap()));
        Some(Entry::from(raw))
    }
}

#[must_use]
pub fn entries_to_offset(entries: u32) -> u64 {
    u64::from(entries) * u64::try_from(size_of::<RawEntry>()).unwrap()
        + u64::try_from(size_of::<Header>()).unwrap()
}

#[must_use]
pub fn offset_to_entries(offset: usize) -> u32 {
    u32::try_from(offset.saturating_sub(size_of::<Header>()) / size_of::<RawEntry>()).unwrap()
}
