use std::{
    fs,
    fs::File,
    io,
    io::{ErrorKind, Read as StdRead, Write},
    mem,
    os::fd::{AsRawFd, OwnedFd},
    path::PathBuf,
    ptr,
};

use arrayvec::ArrayVec;
use io_uring::{
    buf_ring::BufRing,
    cqueue::{buffer_select, more, Entry},
    opcode::{AcceptMulti, Close, PollAdd, Read, RecvMsgMulti, SendMsg, Shutdown},
    squeue::Flags,
    types::{Fixed, RecvMsgOutMut},
    IoUring,
};
use log::{debug, info, trace, warn};
use ringboard_core::{dirs::socket_file, IoErr};
use rustix::{
    io::Errno,
    net::{bind_unix, listen, socket, AddressFamily, RecvFlags, SocketAddrUnix, SocketType},
};

use crate::{
    allocator::Allocator,
    requests,
    send_msg_bufs::{SendMsgBufs, Token},
    CliError,
};

const MAX_NUM_CLIENTS_SHIFT: u32 = 5;
const MAX_NUM_CLIENTS: u32 = 1 << MAX_NUM_CLIENTS_SHIFT;
const URING_ENTRIES: u32 = MAX_NUM_CLIENTS * 3;

#[derive(Default, Debug)]
struct Clients {
    connections: u32,
    pending_closes: u32,
    dropped: ArrayVec<(u32, u64), { MAX_NUM_CLIENTS as usize }>,
}

impl Clients {
    const fn is_connected(&self, id: u32) -> bool {
        debug_assert!(id < u32::BITS);
        (self.connections & (1 << id)) != 0
    }

    const fn is_closing(&self, id: u32) -> bool {
        debug_assert!(id < u32::BITS);
        (self.pending_closes & (1 << id)) != 0
    }

    fn set_connected(&mut self, id: u32) {
        debug_assert!(id < u32::BITS);
        self.connections |= 1 << id;
        self.pending_closes &= !(1 << id);
    }

    fn set_disconnected(&mut self, id: u32) {
        debug_assert!(id < u32::BITS);
        self.connections &= !(1 << id);
        self.pending_closes |= 1 << id;
    }

    fn set_closed(&mut self, id: u32) {
        debug_assert!(id < u32::BITS);
        self.connections &= !(1 << id);
        self.pending_closes &= !(1 << id);
    }

    fn add_dropped(&mut self, id: u32, data: u64) {
        debug_assert!(id < u32::BITS);
        self.dropped.push((id, data));
    }

    fn pop_dropped(&mut self) -> Option<(u32, u64)> {
        self.dropped.pop()
    }
}

fn setup_uring() -> Result<(IoUring, BufRing), CliError> {
    let uring = IoUring::<io_uring::squeue::Entry>::builder()
        .setup_coop_taskrun()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(URING_ENTRIES)
        .map_io_err(|| "Failed to create io_uring.")?;

    let signal_handler = unsafe {
        use std::os::fd::FromRawFd;

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

    let low_mem_listener = {
        let mut cgroup = String::from("/sys/fs/cgroup");
        let start = cgroup.len();
        File::open("/proc/self/cgroup")
            .map_io_err(|| "Failed to open cgroup file: \"/proc/self/cgroup\"")?
            .read_to_string(&mut cgroup)
            .map_io_err(|| "Failed to read cgroup file: \"/proc/self/cgroup\"")?;
        if let Some((idx, _)) = cgroup.match_indices(':').nth(1) {
            cgroup.replace_range(start..=idx, "");
        }
        cgroup.truncate(cgroup.trim_end().len());

        let mut mem_pressure_path = PathBuf::from(cgroup);
        mem_pressure_path.push("memory.pressure");
        let mut mem_pressure = File::options()
            .read(true)
            .write(true)
            .open(&mem_pressure_path)
            .map_io_err(|| format!("Failed to open pressure file: {mem_pressure_path:?}"))?;

        mem_pressure
            .write_all(b"some 50000 2000000")
            .map_io_err(|| format!("Failed to write to pressure file: {mem_pressure_path:?}"))?;

        OwnedFd::from(mem_pressure)
    };

    let socket = {
        let socket_file = socket_file();
        let addr = {
            match fs::remove_file(&socket_file) {
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                r => r,
            }
            .map_io_err(|| format!("Failed to remove old socket: {socket_file:?}"))?;

            SocketAddrUnix::new(&socket_file)
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

    let built_ins = [
        socket.as_raw_fd(),
        signal_handler.as_raw_fd(),
        low_mem_listener.as_raw_fd(),
    ];
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
        .register_buf_ring(u16::try_from(MAX_NUM_CLIENTS * 2).unwrap(), 0, 256)
        .map_io_err(|| "Failed to register buffer ring with io_uring.")?;

    Ok((uring, buf_ring))
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub fn run(allocator: &mut Allocator) -> Result<(), CliError> {
    const REQ_TYPE_ACCEPT: u64 = 0;
    const REQ_TYPE_RECV: u64 = 1;
    const REQ_TYPE_CLOSE: u64 = 2;
    const REQ_TYPE_READ_SIGNALS: u64 = 3;
    const REQ_TYPE_SENDMSG: u64 = 4;
    const REQ_TYPE_SHUTDOWN_CONN: u64 = 5;
    const REQ_TYPE_LOW_MEM: u64 = 6;
    const REQ_TYPE_MASK: u64 = 0b111;
    const REQ_TYPE_SHIFT: u32 = REQ_TYPE_MASK.count_ones();

    let accept = AcceptMulti::new(Fixed(MAX_NUM_CLIENTS))
        .allocate_file_index(true)
        .build()
        .user_data(REQ_TYPE_ACCEPT);
    let poll_low_mem = PollAdd::new(
        Fixed(MAX_NUM_CLIENTS + 2),
        u32::try_from(libc::POLLPRI).unwrap(),
    )
    .multi(true)
    .build()
    .user_data(REQ_TYPE_LOW_MEM);
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

    let close = |fd| {
        Close::new(Fixed(fd))
            .build()
            .flags(Flags::SKIP_SUCCESS)
            .user_data(REQ_TYPE_CLOSE | store_fd(fd))
    };

    let (mut uring, mut buf_ring) = setup_uring()?;

    {
        let read_signals = Read::new(Fixed(MAX_NUM_CLIENTS + 1), ptr::null_mut(), 0)
            .buf_group(0)
            .build()
            .flags(Flags::BUFFER_SELECT)
            .user_data(REQ_TYPE_READ_SIGNALS);

        let mut submission = uring.submission();
        unsafe {
            submission
                .push_multiple(&[accept.clone(), read_signals, poll_low_mem.clone()])
                .unwrap();
        }
    }

    info!("Server event loop started.");

    let mut bufs = buf_ring.submissions();
    let mut freed_bufs = false;
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
                    debug!("Handling accept completion.");
                    let result = result.map_io_err(|| "Failed to accept socket connection.")?;
                    if !more(entry.flags()) {
                        pending_entries.push(accept.clone());
                    }
                    pending_entries
                        .push(recvmsg(result).user_data(REQ_TYPE_RECV | store_fd(result)));
                    debug_assert_eq!(0, result >> MAX_NUM_CLIENTS_SHIFT);
                }
                REQ_TYPE_RECV => 'recv: {
                    debug!("Handling recv completion.");
                    let fd = restore_fd(&entry);
                    match result {
                        Err(e)
                            if [Errno::MSGSIZE, Errno::NOBUFS]
                                .iter()
                                .any(|kind| e.raw_os_error() == Some(kind.raw_os_error())) =>
                        {
                            if freed_bufs {
                                pending_entries.push(recvmsg(fd).user_data(entry.user_data()));
                                freed_bufs = false;
                            } else {
                                warn!("No buffers available to receive client {fd}'s message.");
                                clients.add_dropped(fd, entry.user_data());
                            }
                            break 'recv;
                        }
                        Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                            warn!("Client {fd} reset the connection.");
                            pending_entries.push(close(fd));

                            clients.set_closed(fd);
                            break 'recv;
                        }
                        r => r.map_io_err(|| format!("Failed to recv from client {fd}."))?,
                    };

                    debug_assert!(buffer_select(entry.flags()).is_some());
                    let mut buf = unsafe {
                        bufs.get(entry.flags(), usize::try_from(entry.result()).unwrap())
                    };
                    let msg = RecvMsgOutMut::parse(&mut buf, &receive_hdr).map_err(|()| {
                        CliError::Internal {
                            context: "Didn't allocate enough large enough buffers.".into(),
                        }
                    })?;
                    if msg.is_name_data_truncated()
                        || msg.is_control_data_truncated()
                        || msg.is_payload_truncated()
                    {
                        return Err(CliError::Internal {
                            context: "Received data was truncated.".into(),
                        });
                    }

                    if msg.payload_data.is_empty() {
                        if clients.is_closing(fd) {
                            info!("Client {fd} shut down.");
                        } else {
                            info!("Client {fd} closed the connection.");
                            pending_entries.push(close(fd));
                        }

                        clients.set_closed(fd);
                    } else {
                        if clients.is_closing(fd) {
                            warn!("Dropping spurious message for client {fd}.");
                            break 'recv;
                        }

                        if !more(entry.flags()) {
                            pending_entries.push(recvmsg(fd).user_data(entry.user_data()));
                        }

                        let response = if clients.is_connected(fd) {
                            requests::handle(
                                msg.payload_data,
                                msg.control_data,
                                &mut send_bufs,
                                allocator,
                            )?
                        } else {
                            let (version_valid, resp) =
                                requests::connect(msg.payload_data, &mut send_bufs)?;
                            if version_valid {
                                info!("Client {fd} connected.");
                                clients.set_connected(fd);
                            } else {
                                clients.set_disconnected(fd);
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
                                            | (u64::from(buf.into_index())
                                                << (REQ_TYPE_SHIFT + Token::BITS))
                                            | store_fd(fd),
                                    ),
                            );
                        }

                        if !clients.is_connected(fd) {
                            pending_entries.push(
                                Shutdown::new(Fixed(fd), libc::SHUT_RDWR)
                                    .build()
                                    .flags(Flags::IO_LINK | Flags::SKIP_SUCCESS)
                                    .user_data(REQ_TYPE_SHUTDOWN_CONN | store_fd(fd)),
                            );
                            pending_entries.push(close(fd));
                        }
                    }
                }
                REQ_TYPE_SENDMSG => {
                    debug!("Handling sendmsg completion.");
                    {
                        let token = entry.user_data() >> REQ_TYPE_SHIFT;
                        unsafe {
                            send_bufs.free(token);
                        }
                    }
                    {
                        let index = entry.user_data() >> (REQ_TYPE_SHIFT + u8::BITS);
                        let index = u16::try_from(index & u64::from(u16::MAX)).unwrap();
                        unsafe {
                            bufs.recycle_by_index(index);
                        }
                        freed_bufs = true;
                    }

                    if let Some((fd, data)) = clients.pop_dropped() {
                        info!("Restoring client {fd}'s connection.");
                        pending_entries.push(recvmsg(fd).user_data(data));
                    }

                    let fd = restore_fd(&entry);
                    match result {
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
                            ) =>
                        {
                            warn!("Client {fd} forcefully disconnected: {e:?}");
                        }
                        r => {
                            r.map_io_err(|| format!("Failed to send response to client {fd}."))?;
                        }
                    }
                }
                REQ_TYPE_SHUTDOWN_CONN => {
                    debug!("Handling connection shutdown completion.");
                    let fd = restore_fd(&entry);
                    result.map_io_err(|| format!("Failed to cancel recv for client {fd}."))?;
                }
                REQ_TYPE_CLOSE => {
                    debug!("Handling close completion.");
                    let fd = restore_fd(&entry);
                    result.map_io_err(|| format!("Failed to close client {fd}."))?;
                }
                REQ_TYPE_READ_SIGNALS => {
                    debug!("Handling read_signals completion.");
                    match result {
                        Err(e) if e.raw_os_error() == Some(Errno::NOBUFS.raw_os_error()) => {
                            // We don't actually care about what's in the
                            // buffer, so carry on.
                        }
                        r => {
                            r.map_io_err(|| "Failed to read signal.")?;

                            debug_assert!(buffer_select(entry.flags()).is_some());
                            unsafe {
                                bufs.recycle(entry.flags());
                            }
                        }
                    }

                    break 'outer;
                }
                REQ_TYPE_LOW_MEM => {
                    debug!("Handling low memory completion.");
                    let result = result.map_io_err(|| "Failed to poll.")?;

                    if !more(entry.flags()) {
                        pending_entries.push(poll_low_mem.clone());
                    }

                    if (result & u32::try_from(libc::POLLERR).unwrap()) != 0 {
                        return Err(CliError::Internal {
                            context: "Error polling for low memory events".into(),
                        });
                    } else if (result & u32::try_from(libc::POLLPRI).unwrap()) != 0 {
                        send_bufs.trim();
                        allocator.trim();
                    } else {
                        return Err(CliError::Internal {
                            context: format!("Unknown low memory poll event received: {result}")
                                .into(),
                        });
                    }
                }
                _ => {
                    return Err(CliError::Internal {
                        context: format!("Unknown request: {}", entry.user_data()).into(),
                    });
                }
            }
        }
        bufs.sync();

        trace!("Queueing entries: {pending_entries:?}");
        let mut submission = uring.submission();
        unsafe { submission.push_multiple(&pending_entries) }.map_err(|_| CliError::Internal {
            context: "Didn't allocate enough io_uring slots.".into(),
        })?;
    }
    Ok(())
}
