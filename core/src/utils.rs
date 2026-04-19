use std::{
    ffi::{CStr, OsStr},
    fmt::Debug,
    fs::File,
    io,
    io::{BorrowedBuf, BorrowedCursor, ErrorKind, Write},
    mem::{MaybeUninit, size_of},
    num::NonZeroI32,
    os::{
        fd::{AsFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
    ptr, slice,
    str::FromStr,
};

use arrayvec::{ArrayString, ArrayVec};
use itoa::Integer;
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{
        AtFlags, CWD, FlockOperation, Mode, OFlags, StatxFlags, copy_file_range, flock, linkat,
        openat, statx, unlinkat,
    },
    io::{Errno, pread},
    net::{AddressFamily, SocketAddrUnix, SocketType, bind, listen, socket},
    path::{Arg, DecInt},
    process::{
        Pid, PidfdFlags, RawPid, Signal, getpid, kill_process, pidfd_open, pidfd_send_signal,
    },
};

use crate::{
    Error, IoErr, Result,
    protocol::{RingKind, composite_id},
};

#[must_use]
pub fn is_plaintext_mime(mime: &str) -> bool {
    const TEXT_MIMES: &[&str] = &[
        "",
        "text",
        "string",
        "utf8_string",
        "text/plain",
        "text/plain;charset=utf-8",
        "text/plain;charset=us-ascii",
        "text/plain;charset=unicode",
    ];

    TEXT_MIMES.iter().any(|b| mime.eq_ignore_ascii_case(b))
}

pub const NUM_BUCKETS: usize = 11;

// The max composite ID is 2^40 (8 bit ring ID and 32 bit entry ID)
pub const DIRECT_FILE_NAME_LEN: usize = "1099511627776".len();

fn read_lock_file_pid(lock_file: impl Copy + Debug, file: &File) -> Result<Pid> {
    let mut pid = [MaybeUninit::uninit(); RawPid::MAX_STR_LEN];
    let mut pid = BorrowedBuf::from(pid.as_mut_slice());
    read_at_to_end(file, pid.unfilled(), 0)
        .map_io_err(|| format!("Failed to read lock file: {lock_file:?}"))?;
    let pid = pid.filled();

    let pid = pid
        .as_str()
        .map_io_err(|| format!("Lock file {lock_file:?} corrupted: {pid:?}"))?
        .trim();
    let pid = NonZeroI32::from_str(pid).map_err(|error| Error::InvalidPidError {
        error,
        context: format!("Lock file {lock_file:?} contains invalid PID: {pid:?}").into(),
    })?;
    Ok(Pid::from_raw(pid.get()).unwrap())
}

mod lock_owned {
    use std::os::fd::OwnedFd;

    use rustix::process::Pid;

    #[derive(Copy, Clone, Debug)]
    pub enum LockAlreadyOwnedActionKind {
        SendQuitAndWait,
        SendKillAndTakeover,
        LeaveBe,
    }

    #[must_use]
    pub struct OwnedLockFile(#[allow(dead_code)] pub(super) OwnedFd);

    pub trait LockAlreadyOwnedAction {
        const KIND: LockAlreadyOwnedActionKind;

        type Output;

        fn locked(lock: OwnedLockFile) -> Self::Output;
        fn exists(output: Pid) -> Self::Output;
    }

    pub struct SendQuitAndWait;
    impl LockAlreadyOwnedAction for SendQuitAndWait {
        const KIND: LockAlreadyOwnedActionKind = LockAlreadyOwnedActionKind::SendQuitAndWait;
        type Output = OwnedLockFile;
        fn locked(lock: OwnedLockFile) -> Self::Output {
            lock
        }
        fn exists(_: Pid) -> Self::Output {
            unreachable!()
        }
    }

    pub struct SendKillAndTakeover;
    impl LockAlreadyOwnedAction for SendKillAndTakeover {
        const KIND: LockAlreadyOwnedActionKind = LockAlreadyOwnedActionKind::SendKillAndTakeover;
        type Output = OwnedLockFile;
        fn locked(lock: OwnedLockFile) -> Self::Output {
            lock
        }
        fn exists(_: Pid) -> Self::Output {
            unreachable!()
        }
    }

    pub struct LeaveBe;
    impl LockAlreadyOwnedAction for LeaveBe {
        const KIND: LockAlreadyOwnedActionKind = LockAlreadyOwnedActionKind::LeaveBe;
        type Output = Result<OwnedLockFile, Pid>;
        fn locked(lock: OwnedLockFile) -> Self::Output {
            Ok(lock)
        }
        fn exists(output: Pid) -> Self::Output {
            Err(output)
        }
    }
}
pub use lock_owned::{LeaveBe, OwnedLockFile, SendKillAndTakeover, SendQuitAndWait};
use lock_owned::{LockAlreadyOwnedAction, LockAlreadyOwnedActionKind};

pub fn acquire_lock_file<A: LockAlreadyOwnedAction>(path: &Path, _: A) -> Result<A::Output> {
    let dir_path = path.parent().ok_or_else(|| Error::Io {
        error: io::Error::new(ErrorKind::IsADirectory, "Invalid lock file path"),
        context: format!("Path {path:?} is not a file").into(),
    })?;
    let dir = openat(
        CWD,
        if dir_path == Path::new("") {
            Path::new(".")
        } else {
            dir_path
        },
        OFlags::DIRECTORY,
        Mode::empty(),
    )
    .map_io_err(|| format!("Failed to open lock file directory: {dir_path:?}"))?;
    flock(&dir, FlockOperation::LockExclusive)
        .map_io_err(|| format!("Failed to acquire lock on lock file directory: {dir_path:?}"))?;

    let create_lock_file = || {
        openat(
            &dir,
            path,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
            Mode::RUSR | Mode::WUSR,
        )
    };
    let me = getpid();
    let init_lock_file = |file: OwnedFd| {
        let mut file = File::from(file);
        let mut buf = ArrayVec::<_, { RawPid::MAX_STR_LEN + 1 }>::new_const();
        writeln!(buf, "{}", me.as_raw_nonzero()).unwrap();
        file.write_all(&buf)
            .map_io_err(|| format!("Failed to write to lock file: {path:?}"))?;
        let file = OwnedFd::from(file);

        flock(&file, FlockOperation::NonBlockingLockShared)
            .map_io_err(|| format!("Failed to acquire lock on lock file: {path:?}"))?;
        Ok(A::locked(OwnedLockFile(file)))
    };

    match create_lock_file() {
        Err(Errno::EXIST) => {}
        r => {
            return r
                .map_io_err(|| format!("Failed to create lock file: {path:?}"))
                .and_then(init_lock_file);
        }
    }

    let prev_lock_file = match openat(&dir, path, OFlags::RDONLY, Mode::empty()) {
        Err(Errno::NOENT) => {
            return create_lock_file()
                .map_io_err(|| format!("Failed to create lock file: {path:?}"))
                .and_then(init_lock_file);
        }
        r => r.map_io_err(|| format!("Failed to open previous lock file: {path:?}"))?,
    };
    let prev_lock_file = File::from(prev_lock_file);
    let pid = {
        match flock(&prev_lock_file, FlockOperation::NonBlockingLockExclusive) {
            Err(Errno::WOULDBLOCK) => Some(read_lock_file_pid(path, &prev_lock_file)?),
            r => {
                r.map_io_err(|| {
                    format!("Failed to check validity of existing lock file's ownership: {path:?}")
                })?;
                None
            }
        }
    };

    if let Some(pid) = pid {
        if pid == me {
            flock(&prev_lock_file, FlockOperation::NonBlockingLockShared).map_io_err(|| {
                format!("Failed to acquire lock on lock file: {prev_lock_file:?}")
            })?;
            return Ok(A::locked(OwnedLockFile(OwnedFd::from(prev_lock_file))));
        }

        'ready: {
            match A::KIND {
                LockAlreadyOwnedActionKind::SendQuitAndWait => {
                    let fd = match pidfd_open(pid, PidfdFlags::empty()) {
                        Err(Errno::SRCH) => break 'ready,
                        r => r.map_io_err(|| format!("Failed to open pid file: {pid:?}"))?,
                    };

                    match pidfd_send_signal(&fd, Signal::QUIT) {
                        Err(Errno::SRCH) => break 'ready,
                        r => r.map_io_err(|| {
                            format!("Failed to send quit to lock file {path:?} owner: {pid:?}")
                        })?,
                    }

                    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                    poll(&mut fds, None).map_io_err(|| {
                        format!("Failed to wait for lock file {path:?} owner to quit: {pid:?}")
                    })?;
                    if !fds[0].revents().contains(PollFlags::IN) {
                        return Err(Error::Io {
                            error: io::Error::new(ErrorKind::InvalidInput, "Bad poll response."),
                            context: "Failed to receive PID poll response.".into(),
                        });
                    }
                }
                LockAlreadyOwnedActionKind::SendKillAndTakeover => {
                    match kill_process(pid, Signal::TERM) {
                        Err(Errno::SRCH) => {
                            // Already dead
                        }
                        r => r.map_io_err(|| {
                            format!("Failed to kill lock file {path:?} owner: {pid:?}")
                        })?,
                    }
                }
                LockAlreadyOwnedActionKind::LeaveBe => return Ok(A::exists(pid)),
            }
        }
    }

    match unlinkat(&dir, path, AtFlags::empty()) {
        Err(Errno::NOENT) => (),
        r => {
            r.map_io_err(|| format!("Failed to remove previous lock file: {path:?}"))?;
        }
    }

    create_lock_file()
        .map_io_err(|| format!("Failed to create lock file: {path:?}"))
        .and_then(init_lock_file)
}

const PROC_SELF_FD_BUF_LEN: usize = "/proc/self/fd/".len() + RawFd::MAX_STR_LEN + 1;

#[inline]
pub fn proc_self_fd_buf<'a, Fd: AsFd>(
    buf: &'a mut ArrayVec<u8, PROC_SELF_FD_BUF_LEN>,
    fd: &Fd,
) -> &'a CStr {
    buf.try_extend_from_slice(b"/proc/self/fd/").unwrap();

    let fd_bytes = DecInt::from_fd(fd);
    let fd_bytes = fd_bytes.as_bytes_with_nul();
    buf.try_extend_from_slice(fd_bytes).unwrap();

    unsafe { CStr::from_bytes_with_nul_unchecked(buf) }
}

pub fn link_tmp_file<Fd: AsFd, DirFd: AsFd, P: Arg>(
    tmp_file: Fd,
    dirfd: DirFd,
    path: P,
) -> rustix::io::Result<()> {
    let mut buf = ArrayVec::new_const();
    linkat(
        CWD,
        proc_self_fd_buf(&mut buf, &tmp_file),
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
        }
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
        let bytes_copied = copy_file_range(
            &fd_in,
            off_in.as_deref_mut(),
            &fd_out,
            off_out.as_deref_mut(),
            len - total_copied,
        )?;
        total_copied += bytes_copied;

        if total_copied == len || bytes_copied == 0 {
            break;
        }
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

pub fn init_unix_server<P: AsRef<OsStr>>(socket_name: P, kind: SocketType) -> Result<OwnedFd> {
    let socket_name = socket_name.as_ref();
    let addr = SocketAddrUnix::new_abstract_name(socket_name.as_bytes())
        .map_io_err(|| format!("Failed to make socket address: {socket_name:?}"))?;

    let socket = socket(AddressFamily::UNIX, kind, None)
        .map_io_err(|| format!("Failed to create socket: {socket_name:?}"))?;
    bind(&socket, &addr).map_io_err(|| format!("Failed to bind socket: {socket_name:?}"))?;
    if kind != SocketType::DGRAM {
        listen(&socket, -1)
            .map_io_err(|| format!("Failed to listen for clients: {socket_name:?}"))?;
    }
    Ok(socket)
}

pub fn read_at_to_end<Fd: AsFd>(
    file: Fd,
    mut buf: BorrowedCursor,
    offset: u64,
) -> rustix::io::Result<()> {
    loop {
        if buf.capacity() == 0 {
            break Ok(());
        }
        match {
            let offset = offset + u64::try_from(buf.written()).unwrap();
            pread(&file, unsafe { buf.as_mut() }, offset)
        } {
            Ok(([], _)) => break Ok(()),
            Ok((init, _)) => {
                let n = init.len();
                unsafe {
                    buf.advance(n);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    }
}
