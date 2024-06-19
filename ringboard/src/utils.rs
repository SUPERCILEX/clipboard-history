use std::{
    ffi::CStr,
    fmt::{Debug, Formatter},
    fs::File,
    io::{BorrowedBuf, ErrorKind::UnexpectedEof, Read, Write},
    marker::PhantomData,
    mem::{size_of, MaybeUninit},
    ops::Deref,
    os::fd::{AsFd, OwnedFd},
    ptr, slice,
    str::FromStr,
};

use arrayvec::{ArrayString, ArrayVec};
use rustix::{
    fs::{copy_file_range, openat, statx, AtFlags, Mode, OFlags, StatxFlags},
    path::Arg,
    process::Pid,
};

use crate::{
    protocol::{composite_id, RingKind},
    Error, IoErr, Result,
};

pub const TEXT_MIMES: &[&str] = &[
    "",
    "text",
    "string",
    "utf8_string",
    "text/plain",
    "text/plain;charset=utf-8",
    "text/plain;charset=us-ascii",
    "text/plain;charset=unicode",
];

pub fn read_server_pid<Fd: AsFd, P: Arg + Copy + Debug>(
    dir: Fd,
    lock_file: P,
) -> Result<Option<Pid>> {
    let file = openat(dir, lock_file, OFlags::RDONLY, Mode::empty())
        .map_io_err(|| format!("Failed to open server lock file: {lock_file:?}"))?;
    let mut file = File::from(file);

    let mut pid = [MaybeUninit::uninit(); 10]; // 2^32 is 10 chars
    let mut pid = BorrowedBuf::from(pid.as_mut_slice());
    match file.read_buf_exact(pid.unfilled()) {
        Err(e) if e.kind() == UnexpectedEof => Ok(()),
        r => r,
    }
    .map_io_err(|| format!("Failed to read server lock file: {lock_file:?}"))?;
    let pid = pid.filled();

    let pid = pid
        .as_str()
        .map_io_err(|| format!("Server lock file corrupted: {pid:?}"))?
        .trim();
    if pid.is_empty() {
        Ok(None)
    } else {
        let pid = i32::from_str(pid).map_err(|error| Error::InvalidPidError {
            error,
            context: format!("Server lock file contains invalid PID: {pid:?}").into(),
        })?;
        Ok(Pid::from_raw(pid))
    }
}

pub trait AsBytes: Sized {
    fn as_bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(ptr::from_ref::<Self>(self).cast(), size_of::<Self>()) }
    }
}

pub fn copy_file_range_all<InFd: AsFd, OutFd: AsFd>(
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

pub fn open_buckets<F: FnMut(&str) -> Result<OwnedFd>>(
    mut open: F,
) -> Result<([OwnedFd; 11], [u64; 11])> {
    let mut buckets = ArrayVec::new_const();

    buckets.push(open("(0, 4]")?);
    for end in 3..12 {
        use std::fmt::Write;

        let start = end - 1;

        let mut buf = ArrayString::<{ "(1024, 2048]".len() }>::new_const();
        write!(buf, "({}, {}]", 1 << start, 1 << end).unwrap();
        buckets.push(open(&buf)?);
    }
    buckets.push(open("(2048, 4096)")?);

    let mut lengths = ArrayVec::new_const();
    for bucket in &buckets {
        lengths.push(
            statx(bucket, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                .map_io_err(|| "Failed to statx bucket.")?
                .stx_size,
        );
    }

    Ok((buckets.into_inner().unwrap(), lengths.into_inner().unwrap()))
}

#[must_use]
pub fn size_to_bucket(bytes: u32) -> u8 {
    u8::try_from(
        bytes
            .saturating_sub(1)
            .checked_ilog2()
            .unwrap_or(0)
            .saturating_sub(1),
    )
    .unwrap()
}

#[must_use]
pub const fn bucket_to_length(bucket: usize) -> u32 {
    1 << (bucket + 2)
}

pub struct DirectFileNameToken<'a, T>(&'a mut [u8], PhantomData<T>);

impl<T> Deref for DirectFileNameToken<'_, T> {
    type Target = CStr;

    fn deref(&self) -> &Self::Target {
        unsafe { CStr::from_ptr(self.0.as_ptr().cast()) }
    }
}

impl<T> Debug for DirectFileNameToken<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        std::str::from_utf8(&self.0[..self.0.len() - 1])
            .unwrap()
            .fmt(f)
    }
}

pub fn direct_file_name(
    buf: &mut [u8; "1099511627776".len() + 1],
    to: RingKind,
    id: u32,
) -> DirectFileNameToken<()> {
    write!(buf.as_mut_slice(), "{:0>13}\0", composite_id(to, id)).unwrap();
    DirectFileNameToken(buf.as_mut_slice(), PhantomData)
}
