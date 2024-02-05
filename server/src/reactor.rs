use std::{
    fs, io,
    io::ErrorKind,
    mem,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    ptr, slice,
};

use arrayvec::ArrayVec;
use clipboard_history_core::{protocol::Request, IoErr};
use io_uring::{
    buf_ring::BufRing,
    cqueue::{buffer_select, more, Entry},
    opcode::{AcceptMulti, AsyncCancel2, Close, Read, RecvMsgMulti, SendMsg},
    squeue::Flags,
    types::{CancelBuilder, Fixed, RecvMsgOut},
    IoUring,
};
use log::{debug, info, warn};
use rustix::net::{
    bind_unix, listen, socket, AddressFamily, RecvFlags, SocketAddrUnix, SocketType,
};

use crate::{requests, send_msg_bufs::SendMsgBufs, CliError};

const MAX_NUM_CLIENTS_SHIFT: u32 = 5;
const MAX_NUM_CLIENTS: u32 = 1 << MAX_NUM_CLIENTS_SHIFT;
const URING_ENTRIES: u32 = MAX_NUM_CLIENTS * 2;

#[derive(Default, Debug)]
struct Clients(u32);

impl Clients {
    fn is_connected(&self, id: u32) -> bool {
        (self.0 & (1 << id)) != 0
    }

    fn set_connected(&mut self, id: u32) {
        self.0 |= 1 << id;
    }

    fn set_disconnected(&mut self, id: u32) {
        self.0 &= !(1 << id);
    }
}

fn setup_uring(socket_file: &Path) -> Result<(IoUring, BufRing), CliError> {
    let uring = IoUring::<io_uring::squeue::Entry>::builder()
        .setup_coop_taskrun()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(URING_ENTRIES)
        .map_io_err(|| "Failed to create io_uring.")?;

    let signal_handler = unsafe {
        use std::os::fd::{FromRawFd, OwnedFd};

        let mut set = mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);

        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGQUIT);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigprocmask(libc::SIG_BLOCK, &set, ptr::null_mut());

        let fd = libc::signalfd(-1, &set, 0);
        if fd < 0 {
            return Err(CliError::Internal {
                context: "Could not create signal fd.".into(),
            });
        }
        OwnedFd::from_raw_fd(fd)
    };

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

    let built_ins = [socket.as_raw_fd(), signal_handler.as_raw_fd()];
    uring
        .submitter()
        .register_files_sparse(MAX_NUM_CLIENTS + u32::try_from(built_ins.len()).unwrap())
        .map_io_err(|| "Failed to set up io_uring fixed file table.")?;
    uring
        .submitter()
        .register_files_update(MAX_NUM_CLIENTS, &built_ins)
        .map_io_err(|| "Failed to register socket FD with io_uring.")?;
    let buf_ring = uring
        .submitter()
        .register_buf_ring(u16::try_from(URING_ENTRIES).unwrap(), 0, 128)
        .map_io_err(|| "Failed to register buffer ring with io_uring.")?;

    Ok((uring, buf_ring))
}

pub fn run(_data_dir: PathBuf, socket_file: &Path) -> Result<(), CliError> {
    const REQ_TYPE_ACCEPT: u64 = 0;
    const REQ_TYPE_RECV: u64 = 1;
    const REQ_TYPE_CLOSE: u64 = 2;
    const REQ_TYPE_READ_SIGNALS: u64 = 3;
    const REQ_TYPE_SENDMSG: u64 = 4;
    const REQ_TYPE_CANCEL_FOR_CLOSE: u64 = 5;
    const REQ_TYPE_MASK: u64 = 0b111;
    const REQ_TYPE_SHIFT: u32 = REQ_TYPE_MASK.count_ones();

    let accept = AcceptMulti::new(Fixed(MAX_NUM_CLIENTS))
        .allocate_file_index(true)
        .build()
        .user_data(REQ_TYPE_ACCEPT);
    let receive_hdr = {
        let mut hdr = unsafe { mem::zeroed::<libc::msghdr>() };
        hdr.msg_controllen = 24;
        hdr
    };
    let recvmsg = |fd| {
        RecvMsgMulti::new(Fixed(fd), &receive_hdr, 0)
            .flags(RecvFlags::TRUNC.bits())
            .build()
    };

    let store_fd = |fd| u64::from(fd) << (u64::BITS - MAX_NUM_CLIENTS_SHIFT);
    let restore_fd = |entry: &Entry| {
        u32::try_from(entry.user_data() >> (u64::BITS - MAX_NUM_CLIENTS_SHIFT)).unwrap()
    };

    let (mut uring, mut buf_ring) = setup_uring(socket_file)?;

    {
        let read_signals = Read::new(Fixed(MAX_NUM_CLIENTS + 1), ptr::null_mut(), 0)
            .buf_group(0)
            .build()
            .flags(Flags::BUFFER_SELECT)
            .user_data(REQ_TYPE_READ_SIGNALS);

        let mut submission = uring.submission();
        unsafe {
            submission
                .push_multiple(&[accept.clone(), read_signals])
                .unwrap();
        }
    }

    info!("Server event loop started.");

    let mut needs_reaccept = false;
    let mut bufs = buf_ring.submissions();
    let mut send_bufs = SendMsgBufs::new();
    let mut clients = Clients::default();
    'outer: loop {
        match uring.submit_and_wait(1) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            r => r,
        }
        .map_io_err(|| "Failed to wait for io_uring.")?;
        let mut pending_entries = ArrayVec::<_, { URING_ENTRIES as usize }>::new();

        for entry in uring.completion() {
            let result = u32::try_from(entry.result())
                .map_err(|_| io::Error::from_raw_os_error(-entry.result()));
            match entry.user_data() & REQ_TYPE_MASK {
                REQ_TYPE_ACCEPT => {
                    info!("Handling accept completion.");
                    let result = result.map_io_err(|| "Failed to accept socket connection.")?;
                    needs_reaccept |= !more(entry.flags());
                    pending_entries
                        .push(recvmsg(result).user_data(REQ_TYPE_RECV | store_fd(result)));
                    debug_assert_eq!(0, result & !MAX_NUM_CLIENTS_SHIFT);
                }
                REQ_TYPE_RECV => 'recv: {
                    info!("Handling recv completion.");
                    let fd = restore_fd(&entry);
                    let result = match result {
                        Err(e) if e.raw_os_error() == Some(125) => {
                            // Cancelled
                            break 'recv;
                        }
                        r => r.map_io_err(|| format!("Failed to recv from client {fd}."))?,
                    };

                    debug_assert!(buffer_select(entry.flags()).is_some());
                    let msg = RecvMsgOut::parse(
                        unsafe { bufs.recycle(entry.flags(), usize::try_from(result).unwrap()) },
                        &receive_hdr,
                    )
                    .map_err(|()| CliError::Internal {
                        context: "Didn't allocate enough large enough buffers.".into(),
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
                        info!("Client {fd} closed the connection.");
                        clients.set_disconnected(fd);
                        pending_entries.push(
                            Close::new(Fixed(fd))
                                .build()
                                .flags(Flags::SKIP_SUCCESS)
                                .user_data(REQ_TYPE_CLOSE | store_fd(fd)),
                        );
                    } else {
                        if !more(entry.flags()) {
                            pending_entries.push(recvmsg(fd).user_data(entry.user_data()));
                        }

                        let response = if clients.is_connected(fd) {
                            requests::handle(
                                unsafe { &*msg.payload_data().as_ptr().cast::<Request>() },
                                unsafe {
                                    slice::from_raw_parts_mut(
                                        msg.control_data().as_ptr().cast_mut(),
                                        msg.control_data().len(),
                                    )
                                },
                                &mut send_bufs,
                            )?
                        } else {
                            let (version_valid, resp) =
                                requests::connect(msg.payload_data(), &mut send_bufs)?;
                            if version_valid {
                                info!("Client {fd} connected.");
                                clients.set_connected(fd);
                            }
                            Some(resp)
                        };
                        if let Some((token, msghdr)) = response {
                            pending_entries.push(
                                SendMsg::new(Fixed(fd), msghdr)
                                    .build()
                                    .flags(if clients.is_connected(fd) {
                                        Flags::empty()
                                    } else {
                                        Flags::IO_LINK
                                    })
                                    .user_data(
                                        REQ_TYPE_SENDMSG
                                            | (u64::from(token) << REQ_TYPE_SHIFT)
                                            | store_fd(fd),
                                    ),
                            )
                        }

                        if !clients.is_connected(fd) {
                            pending_entries.push(
                                AsyncCancel2::new(CancelBuilder::fd(Fixed(fd)))
                                    .build()
                                    .flags(Flags::IO_LINK | Flags::SKIP_SUCCESS)
                                    .user_data(REQ_TYPE_CANCEL_FOR_CLOSE | store_fd(fd)),
                            );
                            pending_entries.push(
                                Close::new(Fixed(fd))
                                    .build()
                                    .flags(Flags::SKIP_SUCCESS)
                                    .user_data(REQ_TYPE_CLOSE | store_fd(fd)),
                            );
                        }
                    }
                }
                REQ_TYPE_SENDMSG => {
                    info!("Handling sendmsg completion.");
                    let token = entry.user_data() >> REQ_TYPE_SHIFT;
                    unsafe {
                        send_bufs.free(token);
                    }

                    let fd = restore_fd(&entry);
                    match result {
                        Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                            warn!("Client {fd} forcefully disconnected: {e:?}");
                        }
                        r => {
                            r.map_io_err(|| format!("Failed to send response to client {fd}."))?;
                        }
                    }
                }
                REQ_TYPE_CANCEL_FOR_CLOSE => {
                    info!("Handling cancel-to-close completion.");
                    let fd = restore_fd(&entry);
                    result.map_io_err(|| format!("Failed to cancel recv for client {fd}."))?;
                }
                REQ_TYPE_CLOSE => {
                    info!("Handling close completion.");
                    let fd = restore_fd(&entry);
                    result.map_io_err(|| format!("Failed to close client {fd}."))?;
                }
                REQ_TYPE_READ_SIGNALS => {
                    info!("Handling read_signals completion.");
                    debug_assert!(buffer_select(entry.flags()).is_some());
                    unsafe { bufs.recycle(entry.flags(), 0) };
                    result.map_io_err(|| "Failed to read signal.")?;

                    break 'outer;
                }
                _ => {
                    return Err(CliError::Internal {
                        context: format!("Unknown request: {}", entry.user_data()).into(),
                    });
                }
            }
        }
        bufs.sync();

        debug!("Queueing entries: {pending_entries:?}");
        let mut submission = uring.submission();
        unsafe { submission.push_multiple(&pending_entries) }.map_err(|_| CliError::Internal {
            context: "Didn't allocate enough io_uring slots.".into(),
        })?;
        if needs_reaccept && unsafe { submission.push(&accept) }.is_ok() {
            debug!("Queueing accept: {accept:?}");
            needs_reaccept = false;
        }
    }
    Ok(())
}
