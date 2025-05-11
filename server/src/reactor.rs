use std::{
    fs::File,
    io,
    io::{ErrorKind, Read as StdRead, Write},
    mem,
    os::fd::{AsRawFd, OwnedFd},
    ptr,
};

use arrayvec::ArrayVec;
use io_uring::{
    IoUring, SubmissionQueue,
    cqueue::{Entry, buffer_select, more},
    opcode::{AcceptMulti, Close, PollAdd, RecvMsgMulti, SendMsg},
    squeue::{Flags, PushError},
    types::Fixed,
};
use log::{debug, info, trace, warn};
use ringboard_core::{IoErr, dirs::socket_file, init_unix_server};
use rustix::{
    fs::{CWD, Mode, OFlags, openat},
    io::Errno,
    net::{RecvFlags, SocketType},
};

use crate::{
    CliError,
    allocator::Allocator,
    io_uring::{buf_ring::BufRing, register_buf_ring, types::RecvMsgOutMut},
    requests,
    send_msg_bufs::SendMsgBufs,
};

pub const MAX_NUM_CLIENTS: u8 = 1 << MAX_NUM_CLIENTS_SHIFT;
pub const MAX_NUM_BUFS_PER_CLIENT: u8 = 8;

const MAX_NUM_CLIENTS_SHIFT: u32 = 5;

#[derive(Default, Debug)]
struct Clients {
    connections: u32,
    pending_closes: u32,
    pending_recv: u32,
    pending_sends: u32,
}

impl Clients {
    fn is_connected(&self, id: u8) -> bool {
        debug_assert!(id < MAX_NUM_CLIENTS);
        (self.connections & (1 << id)) != 0
    }

    fn is_closing(&self, id: u8) -> bool {
        debug_assert!(id < MAX_NUM_CLIENTS);
        (self.pending_closes & (1 << id)) != 0
    }

    fn set_connected(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.connections |= 1 << id;
        self.pending_closes &= !(1 << id);
        self.pending_recv &= !(1 << id);
    }

    const fn set_send_buffered(&mut self, id: u8, value: bool) -> bool {
        let r = (self.pending_sends & (1 << id)) != 0;
        if value {
            self.pending_sends |= 1 << id;
        } else {
            self.pending_sends &= !(1 << id);
        }
        r
    }

    fn set_disconnecting(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.pending_closes |= 1 << id;
    }

    fn set_disconnected(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.connections &= !(1 << id);
        self.pending_closes |= 1 << id;
    }

    fn set_closed(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.connections &= !(1 << id);
        self.pending_closes &= !(1 << id);
        self.pending_recv &= !(1 << id);
    }

    fn set_pending_recv(&mut self, id: u8) {
        debug_assert!(id < MAX_NUM_CLIENTS);
        self.pending_recv |= 1 << id;
    }

    fn take_pending_recv(&mut self, id: u8) -> bool {
        debug_assert!(id < MAX_NUM_CLIENTS);
        let r = (self.pending_recv & (1 << id)) != 0;
        self.pending_recv &= !(1 << id);
        r
    }
}

struct BuiltInFds([u32; 3]);

fn setup_uring() -> Result<(IoUring, BuiltInFds), CliError> {
    let uring = IoUring::<io_uring::squeue::Entry>::builder()
        .setup_coop_taskrun()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build((MAX_NUM_CLIENTS * 2).into())
        .map_io_err(|| "Failed to create io_uring.")?;

    let signal_handler = unsafe {
        use std::os::fd::FromRawFd;

        let mut set = mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&raw mut set);

        libc::sigaddset(&raw mut set, libc::SIGTERM);
        libc::sigaddset(&raw mut set, libc::SIGQUIT);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::sigprocmask(libc::SIG_BLOCK, &raw const set, ptr::null_mut());

        let fd = libc::signalfd(-1, &raw const set, 0);
        if fd < 0 {
            return Err(CliError::Internal {
                context: "Could not create signal fd.".into(),
            });
        }
        OwnedFd::from_raw_fd(fd)
    };

    let low_mem_listener = 'init: {
        let mut mem_pressure_path = Vec::with_capacity(160);
        mem_pressure_path.extend_from_slice(b"/sys/fs/cgr");
        {
            let start = mem_pressure_path.len();
            File::open("/proc/self/cgroup")
                .map_io_err(|| "Failed to open cgroup file: \"/proc/self/cgroup\"")?
                .read_to_end(&mut mem_pressure_path)
                .map_io_err(|| "Failed to read cgroup file: \"/proc/self/cgroup\"")?;
            if !mem_pressure_path[start..].starts_with(b"0::") {
                debug!("Detected cgroup v1 which is unsupported.");
                break 'init None;
            }
            mem_pressure_path[start..start + 3].copy_from_slice(b"oup");
            mem_pressure_path.pop();
        }

        mem_pressure_path.extend_from_slice(b"/memory.pressure");
        let mut mem_pressure = File::from(
            match openat(CWD, &mem_pressure_path, OFlags::RDWR, Mode::empty()) {
                Err(Errno::NOENT | Errno::ACCESS) => {
                    debug!(
                        "Pressure file not available: {}",
                        mem_pressure_path.escape_ascii()
                    );
                    break 'init None;
                }
                r => r.map_io_err(|| {
                    format!(
                        "Failed to open pressure file: {}",
                        mem_pressure_path.escape_ascii()
                    )
                })?,
            },
        );

        mem_pressure
            .write_all(b"some 50000 2000000")
            .map_io_err(|| format!("Failed to write to pressure file: {mem_pressure_path:?}"))?;

        Some(mem_pressure)
    };

    let socket = init_unix_server(socket_file(), SocketType::SEQPACKET)?;

    let (built_ins, built_ins_mapping) = {
        let base = u32::from(MAX_NUM_CLIENTS);
        let mut map = [0; 3];

        let mut fds = ArrayVec::<_, 3>::new_const();
        for (i, &fd) in [
            Some(socket.as_raw_fd()),
            Some(signal_handler.as_raw_fd()),
            low_mem_listener.as_ref().map(File::as_raw_fd),
        ]
        .iter()
        .enumerate()
        {
            let Some(fd) = fd else { continue };
            fds.push(fd);
            map[i] = base + u32::try_from(i).unwrap();
        }

        (fds, BuiltInFds(map))
    };
    uring
        .submitter()
        .register_files_sparse(u32::from(MAX_NUM_CLIENTS) + u32::try_from(built_ins.len()).unwrap())
        .map_io_err(|| "Failed to set up io_uring fixed file table.")?;
    uring
        .submitter()
        .register_files_update(MAX_NUM_CLIENTS.into(), &built_ins)
        .map_io_err(|| "Failed to register socket FD with io_uring.")?;

    Ok((uring, built_ins_mapping))
}

impl From<PushError> for CliError {
    fn from(_: PushError) -> Self {
        Self::Internal {
            context: "Mismanaged io_uring SQEs.".into(),
        }
    }
}

pub fn run(allocator: &mut Allocator) -> Result<(), CliError> {
    const REQ_TYPE_ACCEPT: u64 = 0;
    const REQ_TYPE_RECV: u64 = 1;
    const REQ_TYPE_CLOSE: u64 = 2;
    const REQ_TYPE_READ_SIGNALS: u64 = 3;
    const REQ_TYPE_SENDMSG: u64 = 4;
    const REQ_TYPE_LOW_MEM: u64 = 5;
    const REQ_TYPE_MASK: u64 = 0b111;
    const REQ_TYPE_SHIFT: u32 = REQ_TYPE_MASK.count_ones();

    let (mut uring, BuiltInFds([accept_fd, signal_handler_fd, low_mem_listener_fd])) =
        setup_uring()?;

    #[cfg(feature = "systemd")]
    sd_notify::notify(false, &[sd_notify::NotifyState::Ready])
        .map_io_err(|| "Failed to notify systemd of startup completion.")?;

    let accept = AcceptMulti::new(Fixed(accept_fd))
        .allocate_file_index(true)
        .build()
        .user_data(REQ_TYPE_ACCEPT);
    let poll_low_mem = PollAdd::new(
        Fixed(low_mem_listener_fd),
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
        RecvMsgMulti::new(Fixed(u32::from(fd)), &raw const receive_hdr, u16::from(fd))
            .flags(RecvFlags::TRUNC.bits())
            .build()
    };

    let store_fd = |fd| u64::from(fd) << (u64::BITS - MAX_NUM_CLIENTS_SHIFT);
    let restore_fd = |entry: &Entry| {
        u8::try_from(entry.user_data() >> (u64::BITS - MAX_NUM_CLIENTS_SHIFT)).unwrap()
    };

    let drain_send_bufs = |client: u8,
                           bufs: &mut SendMsgBufs,
                           remaining_submission_slots: usize,
                           submissions: &mut SubmissionQueue|
     -> Result<_, PushError> {
        {
            let mut iter = bufs.drain_pending_sends(client, remaining_submission_slots);
            while let Some((token, msghdr)) = iter.next() {
                trace!("Submitting sendmsg for client {client} at index {token}.");
                let send = SendMsg::new(Fixed(client.into()), msghdr)
                    .build()
                    .flags(if iter.len() > 0 {
                        Flags::IO_LINK
                    } else {
                        Flags::empty()
                    })
                    .user_data(
                        REQ_TYPE_SENDMSG | (u64::from(token) << REQ_TYPE_SHIFT) | store_fd(client),
                    );
                unsafe { submissions.push(&send) }?;
            }
        }
        Ok(bufs.has_pending_sends(client))
    };
    let try_close = |client: u8,
                     clients: &mut Clients,
                     bufs: &mut SendMsgBufs,
                     submissions: &mut SubmissionQueue|
     -> Result<_, PushError> {
        if bufs.has_outstanding_sends(client) {
            clients.set_disconnecting(client);
            return Ok(());
        }

        let close = Close::new(Fixed(u32::from(client)))
            .build()
            .user_data(REQ_TYPE_CLOSE | store_fd(client));
        unsafe { submissions.push(&close) }?;
        clients.set_disconnected(client);

        Ok(())
    };

    {
        let read_signals = PollAdd::new(
            Fixed(signal_handler_fd),
            u32::try_from(libc::POLLIN).unwrap(),
        )
        .build()
        .user_data(REQ_TYPE_READ_SIGNALS);

        let mut submission = uring.submission();
        unsafe {
            submission
                .push_multiple(&[accept.clone(), read_signals])
                .unwrap();
            if low_mem_listener_fd > 0 {
                submission.push(&poll_low_mem).unwrap();
            }
        }
    }

    info!("Server event loop started.");

    let mut sequence_number = 0;
    let mut client_buffers = [const { None::<BufRing> }; MAX_NUM_CLIENTS as usize];
    let mut send_bufs = SendMsgBufs::new();
    let mut clients = Clients::default();
    let mut pending_accept = false;
    let mut clients_with_pending_sends = ArrayVec::<u8, { MAX_NUM_CLIENTS as usize }>::new_const();
    'outer: loop {
        {
            let want = uring.submission().is_empty().into();
            trace!("Waiting for at least {want} events.");
            match uring.submit_and_wait(want) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => r,
            }
            .map_io_err(|| "Failed to wait for io_uring.")?;
        }

        let mut completions = unsafe { uring.completion_shared() };
        let mut submissions = unsafe { uring.submission_shared() };
        loop {
            if submissions.capacity() - submissions.len() < 2 {
                break;
            }
            let Some(entry) = completions.next() else {
                break;
            };

            let result = u32::try_from(entry.result())
                .map_err(|_| io::Error::from_raw_os_error(-entry.result()));
            match entry.user_data() & REQ_TYPE_MASK {
                REQ_TYPE_ACCEPT => 'accept: {
                    debug!("Handling accept completion.");
                    let client = match result {
                        Err(e) if e.raw_os_error() == Some(Errno::NFILE.raw_os_error()) => {
                            warn!("Too many clients clients connected, dropping connection.");
                            pending_accept = true;
                            break 'accept;
                        }
                        r => r.map_io_err(|| "Failed to accept socket connection.")?,
                    };
                    debug_assert!(client < u32::from(MAX_NUM_CLIENTS));
                    #[allow(clippy::cast_possible_truncation)]
                    let client = client as u8;
                    debug!("Accepting client {client}.");

                    debug_assert!(client_buffers[usize::from(client)].is_none());
                    client_buffers[usize::from(client)] = Some(
                        register_buf_ring(
                            &uring.submitter(),
                            MAX_NUM_BUFS_PER_CLIENT.into(),
                            client.into(),
                            256,
                        )
                        .map_io_err(|| "Failed to register buffer ring with io_uring.")?,
                    );

                    if !more(entry.flags()) {
                        unsafe { submissions.push(&accept) }?;
                    }
                    let recv = recvmsg(client).user_data(REQ_TYPE_RECV | store_fd(client));
                    unsafe { submissions.push(&recv) }?;
                }
                REQ_TYPE_RECV => 'recv: {
                    let fd = restore_fd(&entry);
                    debug!("Handling recv completion for client {fd}.");
                    match result {
                        Err(e)
                            if [Errno::MSGSIZE, Errno::NOBUFS]
                                .iter()
                                .any(|kind| e.raw_os_error() == Some(kind.raw_os_error())) =>
                        {
                            warn!("No buffers available to receive client {fd}'s message.");
                            clients.set_pending_recv(fd);
                            break 'recv;
                        }
                        Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                            warn!("Client {fd} reset the connection.");
                            try_close(fd, &mut clients, &mut send_bufs, &mut submissions)?;
                            break 'recv;
                        }
                        r => r.map_io_err(|| format!("Failed to recv from client {fd}."))?,
                    };

                    debug_assert!(buffer_select(entry.flags()).is_some());
                    let mut buf_submissions = client_buffers[usize::from(fd)]
                        .as_mut()
                        .unwrap()
                        .submissions();
                    let mut buf = unsafe {
                        buf_submissions.get(entry.flags(), usize::try_from(entry.result()).unwrap())
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
                        debug!("Client {fd} closed the connection.");
                        if !clients.is_closing(fd) {
                            try_close(fd, &mut clients, &mut send_bufs, &mut submissions)?;
                        }
                    } else {
                        if clients.is_closing(fd) {
                            debug!("Dropping spurious message for client {fd}.");
                            break 'recv;
                        }

                        if !clients.set_send_buffered(fd, true) {
                            clients_with_pending_sends.push(fd);
                        }
                        let response = if clients.is_connected(fd) {
                            requests::handle(
                                msg.payload_data,
                                msg.control_data,
                                &mut send_bufs,
                                allocator,
                                &mut sequence_number,
                            )?
                        } else {
                            let (version_valid, resp) =
                                requests::connect(msg.payload_data, &mut send_bufs);
                            if version_valid {
                                info!("Client {fd} connected.");
                                clients.set_connected(fd);
                            } else {
                                clients.set_disconnected(fd);
                            }
                            Some(resp)
                        };
                        if let Some(resp) = response {
                            send_bufs.alloc(fd, buf.into_index().into(), resp);
                        }

                        if clients.is_connected(fd) {
                            if !more(entry.flags()) {
                                let recv = recvmsg(fd).user_data(entry.user_data());
                                unsafe { submissions.push(&recv) }?;
                            }
                        } else {
                            try_close(fd, &mut clients, &mut send_bufs, &mut submissions)?;
                        }
                    }
                }
                REQ_TYPE_SENDMSG => {
                    let fd = restore_fd(&entry);
                    debug!("Handling sendmsg completion for client {fd}.");

                    {
                        let token = entry.user_data() >> REQ_TYPE_SHIFT;
                        unsafe {
                            send_bufs.free(fd, token);
                        }

                        let index = u16::try_from(token & u64::from(u16::MAX)).unwrap();
                        let mut submissions = client_buffers[usize::from(fd)]
                            .as_mut()
                            .unwrap()
                            .submissions();
                        unsafe {
                            submissions.recycle_by_index(index);
                        }
                    }

                    match result {
                        Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                            if !clients.is_closing(fd) {
                                debug!(
                                    "Client {fd} closed the connection before consuming all \
                                     responses."
                                );
                                clients.set_disconnecting(fd);
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                            if !clients.is_closing(fd) {
                                warn!("Client {fd} forcefully disconnected.");
                                clients.set_disconnecting(fd);
                            }
                        }
                        Err(e) if e.raw_os_error() == Some(Errno::CANCELED.raw_os_error()) => {
                            debug_assert!(clients.is_closing(fd));
                        }
                        r => {
                            r.map_io_err(|| format!("Failed to send response to client {fd}."))?;
                        }
                    }

                    if clients.is_closing(fd) && clients.is_connected(fd) {
                        try_close(fd, &mut clients, &mut send_bufs, &mut submissions)?;
                    } else if !clients.is_closing(fd)
                        && clients.is_connected(fd)
                        && clients.take_pending_recv(fd)
                    {
                        info!("Restoring client {fd}'s connection.");
                        let recv = recvmsg(fd).user_data(REQ_TYPE_RECV | store_fd(fd));
                        unsafe { submissions.push(&recv) }?;
                    }
                }
                REQ_TYPE_CLOSE => {
                    let fd = restore_fd(&entry);
                    debug!("Handling close completion for client {fd}.");
                    result.map_io_err(|| format!("Failed to close client {fd}."))?;
                    info!("Client {fd} disconnected.");

                    clients.set_closed(fd);
                    if let Some(bufs) = mem::take(&mut client_buffers[usize::from(fd)]) {
                        bufs.unregister(&uring.submitter())
                            .map_io_err(|| "Failed to unregister buffer ring with io_uring.")?;
                    }

                    if pending_accept && clients.pending_closes == 0 {
                        info!("Restoring ability to accept new clients.");
                        unsafe { submissions.push(&accept) }?;
                        pending_accept = false;
                    }
                }
                REQ_TYPE_READ_SIGNALS => {
                    debug!("Handling read_signals completion.");
                    let result = result.map_io_err(|| "Failed to poll for signals.")?;
                    if (result & u32::try_from(libc::POLLIN).unwrap()) == 0 {
                        return Err(CliError::Internal {
                            context: format!("Unknown signal poll event received: {result}").into(),
                        });
                    }

                    break 'outer;
                }
                REQ_TYPE_LOW_MEM => {
                    debug!("Handling low memory completion.");
                    let result = result.map_io_err(|| "Failed to poll for low memory events.")?;

                    if !more(entry.flags()) {
                        unsafe { submissions.push(&poll_low_mem) }?;
                    }

                    if (result & u32::try_from(libc::POLLERR).unwrap()) != 0 {
                        return Err(CliError::Internal {
                            context: "Error polling for low memory events".into(),
                        });
                    } else if (result & u32::try_from(libc::POLLPRI).unwrap()) != 0 {
                        send_bufs.trim();
                    } else {
                        return Err(CliError::Internal {
                            context: format!("Unknown low memory poll event received: {result}")
                                .into(),
                        });
                    }
                }
                _ => unreachable!(),
            }
        }

        let mut remaining_sends = ArrayVec::<u8, { MAX_NUM_CLIENTS as usize }>::new_const();
        for (i, &client) in clients_with_pending_sends.iter().enumerate() {
            if !send_bufs.has_ready_block(client) {
                remaining_sends.push(client);
                continue;
            }

            let has_pending = drain_send_bufs(
                client,
                &mut send_bufs,
                submissions.capacity() - submissions.len(),
                &mut submissions,
            )?;
            if has_pending {
                remaining_sends
                    .try_extend_from_slice(&clients_with_pending_sends[i..])
                    .unwrap();
                break;
            }

            clients.set_send_buffered(client, false);
        }
        clients_with_pending_sends = remaining_sends;
    }
    Ok(())
}
