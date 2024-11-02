use std::{
    ffi::CStr,
    fmt::Debug,
    fs,
    fs::File,
    io,
    io::{BorrowedBuf, ErrorKind, ErrorKind::UnexpectedEof, Read, Write},
    mem::{MaybeUninit, size_of},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd, RawFd},
        unix::fs::FileExt,
    },
    path::Path,
    ptr, slice,
    str::FromStr,
};

use arrayvec::{ArrayString, ArrayVec};
use rustix::{
    fs::{
        AtFlags, CWD, Mode, OFlags, StatxFlags, copy_file_range, linkat, openat, statx, unlinkat,
    },
    io::Errno,
    net::{AddressFamily, SocketAddrUnix, SocketType, bind_unix, listen, socket},
    path::Arg,
    process::Pid,
};

use crate::{
    Error, IoErr, Result,
    protocol::{RingKind, composite_id},
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
pub const NUM_BUCKETS: usize = 11;

// The max composite ID is 2^40 (8 bit ring ID and 32 bit entry ID)
pub const DIRECT_FILE_NAME_LEN: usize = "1099511627776".len();

pub fn read_lock_file_pid<Fd: AsFd, P: Arg + Copy + Debug>(
    dir: Fd,
    lock_file: P,
) -> Result<Option<Pid>> {
    let file = openat(dir, lock_file, OFlags::RDONLY, Mode::empty())
        .map_io_err(|| format!("Failed to open lock file: {lock_file:?}"))?;
    let mut file = File::from(file);

    let mut pid = [MaybeUninit::uninit(); 10]; // 2^32 is 10 chars
    let mut pid = BorrowedBuf::from(pid.as_mut_slice());
    match file.read_buf_exact(pid.unfilled()) {
        Err(e) if e.kind() == UnexpectedEof => Ok(()),
        r => r,
    }
    .map_io_err(|| format!("Failed to read lock file: {lock_file:?}"))?;
    let pid = pid.filled();

    let pid = pid
        .as_str()
        .map_io_err(|| format!("Lock file {lock_file:?} corrupted: {pid:?}"))?
        .trim();
    if pid.is_empty() {
        Ok(None)
    } else {
        let pid = i32::from_str(pid).map_err(|error| Error::InvalidPidError {
            error,
            context: format!("Lock file {lock_file:?} contains invalid PID: {pid:?}").into(),
        })?;
        Ok(Pid::from_raw(pid))
    }
}

pub fn link_tmp_file<Fd: AsFd, DirFd: AsFd, P: Arg>(
    tmp_file: Fd,
    dirfd: DirFd,
    path: P,
) -> rustix::io::Result<()> {
    const _: () = assert!(RawFd::BITS <= i32::BITS);
    let mut buf = [0u8; "/proc/self/fd/".len() + "-2147483648".len() + 1];
    write!(
        buf.as_mut_slice(),
        "/proc/self/fd/{}",
        tmp_file.as_fd().as_raw_fd()
    )
    .unwrap();

    linkat(
        CWD,
        unsafe { CStr::from_ptr(buf.as_ptr().cast()) },
        dirfd,
        path,
        AtFlags::SYMLINK_FOLLOW,
    )
}

pub fn create_tmp_file<Fd: AsFd, P1: Arg, P2: Arg + Copy>(
    tmp_file_unsupported: &mut bool,
    dirfd: Fd,
    path: P1,
    fallback_path: P2,
    oflags: OFlags,
    create_mode: Mode,
) -> rustix::io::Result<OwnedFd> {
    if !*tmp_file_unsupported {
        match openat(&dirfd, path, oflags | OFlags::TMPFILE, create_mode) {
            Err(Errno::NOTSUP) => *tmp_file_unsupported = true,
            r => return r,
        };
    }

    let file = openat(
        &dirfd,
        fallback_path,
        oflags | OFlags::CREATE | OFlags::EXCL,
        create_mode,
    )?;
    unlinkat(dirfd, fallback_path, AtFlags::empty())?;
    Ok(file)
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
) -> Result<([OwnedFd; NUM_BUCKETS], [u64; NUM_BUCKETS])> {
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
pub fn size_to_bucket(bytes: u16) -> u8 {
    u8::try_from(
        bytes
            .saturating_sub(1)
            .checked_ilog2()
            .unwrap_or(0)
            .saturating_sub(1),
    )
    .unwrap()
}

const _: () = assert!(NUM_BUCKETS + 2 < u16::BITS as usize);

#[must_use]
pub const fn bucket_to_length(bucket: usize) -> u16 {
    debug_assert!(bucket < NUM_BUCKETS);
    1 << (bucket + 2)
}

pub fn direct_file_name(
    buf: &mut [MaybeUninit<u8>; DIRECT_FILE_NAME_LEN + 1],
    to: RingKind,
    index: u32,
) -> &CStr {
    let mut buf = BorrowedBuf::from(buf.as_mut_slice());
    write!(buf.unfilled(), "{:0>13}\0", composite_id(to, index)).unwrap();
    unsafe { CStr::from_ptr(buf.filled_mut().as_ptr().cast()) }
}

pub fn init_unix_server<P: AsRef<Path>>(socket_file: P, kind: SocketType) -> Result<OwnedFd> {
    let socket_file = socket_file.as_ref();
    let addr = {
        match fs::remove_file(socket_file) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            r => r,
        }
        .map_io_err(|| format!("Failed to remove old socket: {socket_file:?}"))?;

        if let Some(parent) = socket_file.parent() {
            fs::create_dir_all(parent)
                .map_io_err(|| format!("Failed to create socket directory: {parent:?}"))?;
        }
        SocketAddrUnix::new(socket_file)
            .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?
    };

    let socket = socket(AddressFamily::UNIX, kind, None)
        .map_io_err(|| format!("Failed to create socket: {socket_file:?}"))?;
    bind_unix(&socket, &addr).map_io_err(|| format!("Failed to bind socket: {socket_file:?}"))?;
    if kind != SocketType::DGRAM {
        listen(&socket, -1)
            .map_io_err(|| format!("Failed to listen for clients: {socket_file:?}"))?;
    }
    Ok(socket)
}

pub fn read_at_to_end(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<usize> {
    loop {
        if buf.is_empty() {
            break Ok(buf.len());
        }
        match file.read_at(buf, offset) {
            Ok(0) => break Ok(buf.len()),
            Ok(n) => {
                let tmp = buf;
                buf = &mut tmp[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    }
}
