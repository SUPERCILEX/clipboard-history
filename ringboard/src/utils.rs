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
    fs::{copy_file_range, openat, Mode, OFlags},
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
