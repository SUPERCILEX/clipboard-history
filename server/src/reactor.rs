use std::{
    fs, io,
    io::ErrorKind,
    mem,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use arrayvec::ArrayVec;
use clipboard_history_core::{protocol::Request, IoErr};
use io_uring::{
    buf_ring::BufRing,
    cqueue::{buffer_select, more},
    opcode::{AcceptMulti, Close, RecvMsgMulti},
    squeue::Flags,
    types::{Fixed, RecvMsgOut},
    IoUring,
};
use rustix::net::{
    bind_unix, listen, socket, AddressFamily, RecvFlags, SocketAddrUnix, SocketType,
};

use crate::{handler::handle_payload, CliError};

const MAX_NUM_CLIENTS_SHIFT: u32 = 5;
const MAX_NUM_CLIENTS: u32 = 1 << MAX_NUM_CLIENTS_SHIFT;

fn setup_uring(socket_file: &Path) -> Result<(IoUring, BufRing), CliError> {
    let uring = IoUring::<io_uring::squeue::Entry>::builder()
        .setup_coop_taskrun()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(MAX_NUM_CLIENTS * 2)
        .map_io_err(|| "Failed to create io_uring.")?;

    let socket = {
        let addr = {
            match fs::remove_file(socket_file) {
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                r => r,
            }
            .map_io_err(|| format!("Failed to remove old socket: {socket_file:?}"))?;

            SocketAddrUnix::new(socket_file)
                .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))
        }?;

        let socket = socket(AddressFamily::UNIX, SocketType::SEQPACKET, None)
            .map_io_err(|| format!("Failed to create socket: {socket_file:?}"))?;
        bind_unix(&socket, &addr)
            .map_io_err(|| format!("Failed to bind socket: {socket_file:?}"))?;
        listen(&socket, -1)
            .map_io_err(|| format!("Failed to listen for clients: {socket_file:?}"))?;
        socket
    };

    uring
        .submitter()
        .register_files_sparse(MAX_NUM_CLIENTS)
        .map_io_err(|| "Failed to set up io_uring fixed file table.")?;
    uring
        .submitter()
        .register_files_update(0, &[socket.as_raw_fd()])
        .map_io_err(|| "Failed to register socket FD with io_uring.")?;
    let buf_ring = uring
        .submitter()
        .register_buf_ring(u16::try_from(MAX_NUM_CLIENTS * 2).unwrap(), 0, 64)
        .map_io_err(|| "Failed to register buffer ring with io_uring.")?;

    Ok((uring, buf_ring))
}

pub fn run(_data_dir: PathBuf, socket_file: &Path) -> Result<(), CliError> {
    const REQ_TYPE_ACCEPT: u64 = 0;
    const REQ_TYPE_RECV: u64 = 1;
    const REQ_TYPE_CLOSE: u64 = 2;
    const REQ_TYPE_MASK: u64 = 0b11;

    let accept = AcceptMulti::new(Fixed(0))
        .allocate_file_index(true)
        .build()
        .user_data(REQ_TYPE_ACCEPT);
    let empty_msghdr = {
        let mut hdr = unsafe { mem::zeroed::<libc::msghdr>() };
        hdr.msg_controllen = 24;
        hdr
    };
    let recvmsg = |fd| {
        RecvMsgMulti::new(Fixed(fd), &empty_msghdr, 0)
            .flags(RecvFlags::TRUNC.bits())
            .build()
    };

    let (mut uring, mut buf_ring) = setup_uring(socket_file)?;

    {
        let mut submission = uring.submission();
        unsafe {
            submission.push(&accept).unwrap();
        }
    }

    let mut needs_reaccept = false;
    let mut bufs = buf_ring.submissions();
    loop {
        match uring.submit_and_wait(1) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            r => r,
        }
        .map_io_err(|| "Failed to wait for io_uring.")?;
        let mut pending_entries = ArrayVec::<_, 64>::new();

        for entry in uring.completion() {
            let result = u32::try_from(entry.result())
                .map_err(|_| io::Error::from_raw_os_error(-entry.result()));
            match entry.user_data() & REQ_TYPE_MASK {
                REQ_TYPE_ACCEPT => {
                    let result = result.map_io_err(|| "Failed to accept socket connection.")?;
                    needs_reaccept |= !more(entry.flags());
                    pending_entries.push(recvmsg(result).user_data(
                        REQ_TYPE_RECV | (u64::from(result) << (u64::BITS - MAX_NUM_CLIENTS_SHIFT)),
                    ));
                    debug_assert_eq!(0, result & !MAX_NUM_CLIENTS_SHIFT);
                }
                REQ_TYPE_RECV => {
                    let result = result.map_io_err(|| "Failed to accept recv from socket.")?;
                    let fd =
                        u32::try_from(entry.user_data() >> (u64::BITS - MAX_NUM_CLIENTS_SHIFT))
                            .unwrap();

                    debug_assert!(buffer_select(entry.flags()).is_some());
                    let msg = RecvMsgOut::parse(
                        unsafe { bufs.recycle(entry.flags(), usize::try_from(result).unwrap()) },
                        &empty_msghdr,
                    )
                    .map_err(|()| CliError::Internal {
                        context: "Didn't allocate enough buffer space.".into(),
                    })?;
                    if msg.is_name_data_truncated()
                        || msg.is_control_data_truncated()
                        || msg.is_payload_truncated()
                    {
                        return Err(CliError::Internal {
                            context: "Received data was truncated.".into(),
                        });
                    }

                    if msg.payload_data().is_empty() {
                        pending_entries.push(
                            Close::new(Fixed(fd))
                                .build()
                                .user_data(REQ_TYPE_CLOSE)
                                .flags(Flags::SKIP_SUCCESS),
                        );
                    } else {
                        if !more(entry.flags()) {
                            pending_entries.push(recvmsg(fd).user_data(entry.user_data()));
                        }

                        handle_payload(
                            unsafe { &*msg.payload_data().as_ptr().cast::<Request>() },
                            msg.control_data(),
                        );
                    }
                }
                REQ_TYPE_CLOSE => {
                    result.map_io_err(|| "Failed to close file.")?;
                }
                _ => unreachable!("Unknown request: {}", entry.user_data()),
            }
        }
        bufs.sync();

        let mut submission = uring.submission();
        unsafe {
            submission.push_multiple(&pending_entries).unwrap();
        }
        if needs_reaccept && unsafe { submission.push(&accept) }.is_ok() {
            needs_reaccept = false;
        }
    }
}
