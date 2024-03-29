use std::{
    fmt::Debug,
    fs::File,
    io::{BorrowedBuf, ErrorKind::UnexpectedEof, Read},
    mem::{size_of, MaybeUninit},
    os::fd::AsFd,
    ptr, slice,
    str::FromStr,
};

use rustix::{
    fs::{openat, Mode, OFlags},
    path::Arg,
    process::Pid,
};

use crate::{Error, IoErr, Result};

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
