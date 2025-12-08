use std::{
    ffi::CStr,
    fmt::Debug,
    fs,
    fs::File,
    io,
    io::{BorrowedBuf, BorrowedCursor, ErrorKind, Write},
    mem,
    mem::{MaybeUninit, size_of},
    os::{
        fd::{AsFd, OwnedFd, RawFd},
        unix::fs::FileExt,
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
        Pid, PidfdFlags, Signal, getpid, kill_process, pidfd_open, pidfd_send_signal,
        test_kill_process,
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

pub enum LockFilePid {
    Valid(Pid),
    Deleted,
    UserReset,
}

// 2^32 is 10 chars
const LOCK_FILE_BUF_SIZE: usize = 10;

pub fn read_lock_file_pid(lock_file: impl Copy + Debug, file: &File) -> Result<LockFilePid> {
    let mut pid = [MaybeUninit::uninit(); LOCK_FILE_BUF_SIZE];
    let mut pid = BorrowedBuf::from(pid.as_mut_slice());
    read_at_to_end(file, pid.unfilled(), 0)
        .map_io_err(|| format!("Failed to read lock file: {lock_file:?}"))?;
    let pid = pid.filled();

    if pid.iter().all(|&b| b == b'.') {
        return Ok(LockFilePid::Deleted);
    }

    let pid = pid
        .as_str()
        .map_io_err(|| format!("Lock file {lock_file:?} corrupted: {pid:?}"))?
        .trim();
    if pid.is_empty() {
        Ok(LockFilePid::UserReset)
    } else {
        let pid = i32::from_str(pid).map_err(|error| Error::InvalidPidError {
            error,
            context: format!("Lock file {lock_file:?} contains invalid PID: {pid:?}").into(),
        })?;
        Ok(Pid::from_raw(pid).map_or(LockFilePid::UserReset, LockFilePid::Valid))
    }
}

mod lock_owned {
    use rustix::process::Pid;

    #[derive(Copy, Clone, Debug)]
    pub enum LockAlreadyOwnedActionKind {
        SendQuitAndWait,
        SendKillAndTakeover,
        LeaveBe,
    }

    pub trait LockAlreadyOwnedAction {
        const KIND: LockAlreadyOwnedActionKind;

        type Output;
        const NOTHING: Self::Output;

        fn something(output: Pid) -> Self::Output;
    }

    pub struct SendQuitAndWait;
    impl LockAlreadyOwnedAction for SendQuitAndWait {
        const KIND: LockAlreadyOwnedActionKind = LockAlreadyOwnedActionKind::SendQuitAndWait;
        type Output = ();
        const NOTHING: Self::Output = ();
        fn something(_: Pid) -> Self::Output {
            unreachable!()
        }
    }

    pub struct SendKillAndTakeover;
    impl LockAlreadyOwnedAction for SendKillAndTakeover {
        const KIND: LockAlreadyOwnedActionKind = LockAlreadyOwnedActionKind::SendKillAndTakeover;
        type Output = ();
        const NOTHING: Self::Output = ();
        fn something(_: Pid) -> Self::Output {
            unreachable!()
        }
    }

    pub struct LeaveBe;
    impl LockAlreadyOwnedAction for LeaveBe {
        const KIND: LockAlreadyOwnedActionKind = LockAlreadyOwnedActionKind::LeaveBe;
        type Output = Option<Pid>;
        const NOTHING: Self::Output = None;

        fn something(output: Pid) -> Self::Output {
            Some(output)
        }
    }
}
pub use lock_owned::{LeaveBe, SendKillAndTakeover, SendQuitAndWait};
use lock_owned::{LockAlreadyOwnedAction, LockAlreadyOwnedActionKind};

pub fn acquire_lock_file<
    Fd: AsFd + Copy,
    P1: Arg,
    P2: Arg + Copy,
    P3: Arg + Copy + Debug,
    A: LockAlreadyOwnedAction,
>(
    tmp_file_unsupported: &mut bool,
    dirfd: Fd,
    prepare_path: P1,
    prepare_fallback_path: P2,
    path: P3,
    _: A,
) -> Result<A::Output> {
    let mut lock_file = File::from(
        create_tmp_file(
            tmp_file_unsupported,
            dirfd,
            prepare_path,
            prepare_fallback_path,
            OFlags::WRONLY,
            Mode::RUSR | Mode::WUSR,
        )
        .map_io_err(|| "Failed to prepare lock file.")?,
    );

    let me = getpid();
    writeln!(lock_file, "{}", me.as_raw_nonzero())
        .map_io_err(|| "Failed to write to prepared lock file.")?;

    loop {
        match link_tmp_file(&lock_file, dirfd, path) {
            Err(Errno::EXIST) => {}
            r => {
                return r
                    .map_io_err(|| format!("Failed to materialize lock file: {path:?}"))
                    .map(|()| A::NOTHING);
            }
        }

        let lock_file = 'retry: {
            let lock_file = match openat(dirfd, path, OFlags::RDWR, Mode::empty()) {
                Err(Errno::NOENT) => break 'retry None,
                r => r.map_io_err(|| format!("Failed to open lock file: {path:?}"))?,
            };
            flock(&lock_file, FlockOperation::LockExclusive)
                .map_io_err(|| format!("Failed to acquire lock on lock file: {path:?}"))?;
            let lock_file = File::from(lock_file);

            let pid = match read_lock_file_pid(path, &lock_file)? {
                LockFilePid::Valid(pid) => pid,
                LockFilePid::Deleted => break 'retry None,
                LockFilePid::UserReset => break 'retry Some(lock_file),
            };

            if pid == me {
                return Ok(A::NOTHING);
            }

            match A::KIND {
                LockAlreadyOwnedActionKind::SendQuitAndWait => {
                    let fd = match pidfd_open(pid, PidfdFlags::empty()) {
                        Err(Errno::SRCH) => break 'retry Some(lock_file),
                        r => r.map_io_err(|| format!("Failed to open pid file: {pid:?}"))?,
                    };

                    match pidfd_send_signal(&fd, Signal::QUIT) {
                        Err(Errno::SRCH) => break 'retry None,
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
                    None
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
                    Some(lock_file)
                }
                LockAlreadyOwnedActionKind::LeaveBe => match test_kill_process(pid) {
                    Err(Errno::SRCH) => Some(lock_file),
                    r => {
                        return r
                            .map_io_err(|| {
                                format!("Failed to check lock file {path:?} owner status: {pid:?}")
                            })
                            .map(|()| A::something(pid));
                    }
                },
            }
        };

        if let Some(lock_file) = lock_file {
            lock_file
                .write_all_at(&[b'.'; LOCK_FILE_BUF_SIZE], 0)
                .map_io_err(|| {
                    format!("Failed to write deletion pattern to previous lock file: {path:?}")
                })?;
            unlinkat(dirfd, path, AtFlags::empty())
                .map_io_err(|| format!("Failed to remove previous lock file: {path:?}"))?;
        }
    }
}

const PROC_SELF_FD_BUF_LEN: usize = "/proc/self/fd/".len() + RawFd::MAX_STR_LEN + 1;

#[inline]
#[allow(clippy::transmute_ptr_to_ptr)]
pub fn proc_self_fd_buf<'a, Fd: AsFd>(
    buf: &'a mut [MaybeUninit<u8>; PROC_SELF_FD_BUF_LEN],
    fd: &Fd,
) -> &'a CStr {
    let (header, fd_buf) = buf.split_at_mut("/proc/self/fd/".len());

    header
        .copy_from_slice(unsafe { mem::transmute::<&[u8], &[MaybeUninit<u8>]>(b"/proc/self/fd/") });

    let fd_bytes = DecInt::from_fd(fd);
    let fd_bytes = fd_bytes.as_bytes_with_nul();
    fd_buf[..fd_bytes.len()]
        .copy_from_slice(unsafe { mem::transmute::<&[u8], &[MaybeUninit<u8>]>(fd_bytes) });

    let len = header.len() + fd_bytes.len();
    unsafe {
        CStr::from_bytes_with_nul_unchecked(mem::transmute::<&[MaybeUninit<u8>], &[u8]>(
            &buf[..len],
        ))
    }
}

pub fn link_tmp_file<Fd: AsFd, DirFd: AsFd, P: Arg>(
    tmp_file: Fd,
    dirfd: DirFd,
    path: P,
) -> rustix::io::Result<()> {
    let mut buf = [MaybeUninit::uninit(); PROC_SELF_FD_BUF_LEN];
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
    bind(&socket, &addr).map_io_err(|| format!("Failed to bind socket: {socket_file:?}"))?;
    if kind != SocketType::DGRAM {
        listen(&socket, -1)
            .map_io_err(|| format!("Failed to listen for clients: {socket_file:?}"))?;
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
