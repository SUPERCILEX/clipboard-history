#![feature(slice_split_once)]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

use std::{
    borrow::Cow,
    fs::File,
    io,
    io::{IoSlice, IoSliceMut, Seek, SeekFrom},
    mem::ManuallyDrop,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
};

pub use ring_reader::{DatabaseReader, Entry, EntryReader, Kind, LoadedEntry, RingReader};
pub use ringboard_core as core;
use ringboard_core::{
    protocol,
    protocol::{
        AddResponse, GarbageCollectResponse, IdNotFoundError, MimeType, MoveToFrontResponse,
        RemoveResponse, Request, RingKind, SwapResponse,
    },
    AsBytes, IoErr,
};
use rustix::{
    fs::{openat, statx, AtFlags, FileType, Mode, OFlags, StatxFlags, CWD},
    net::{
        connect_unix, recvmsg, sendmsg_unix, socket_with, AddressFamily, RecvAncillaryBuffer,
        RecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix,
        SocketFlags, SocketType,
    },
};
pub use search::search;
use thiserror::Error;

pub mod duplicate_detection;
mod ring_reader;
pub mod search;

macro_rules! response {
    ($t:ty) => {
        response::<$t, { size_of::<$t>() }>
    };
}

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("{0}")]
    Core(#[from] ringboard_core::Error),
    #[error(
        "Protocol version mismatch: expected {} but got {actual}.",
        protocol::VERSION
    )]
    VersionMismatch { actual: u8 },
    #[error("The server returned an invalid response.")]
    InvalidResponse { context: Cow<'static, str> },
}

impl From<IdNotFoundError> for ClientError {
    fn from(value: IdNotFoundError) -> Self {
        Self::Core(ringboard_core::Error::IdNotFound(value))
    }
}

pub fn connect_to_server(addr: &SocketAddrUnix) -> Result<OwnedFd, ClientError> {
    connect_to_server_with(addr, SocketFlags::empty())
}

pub fn connect_to_server_with(
    addr: &SocketAddrUnix,
    flags: SocketFlags,
) -> Result<OwnedFd, ClientError> {
    let socket = socket_with(AddressFamily::UNIX, SocketType::SEQPACKET, flags, None)
        .map_io_err(|| format!("Failed to create socket: {addr:?}"))?;
    connect_unix(&socket, addr).map_io_err(|| format!("Failed to connect to server: {addr:?}"))?;

    {
        sendmsg_unix(
            &socket,
            addr,
            &[IoSlice::new(&[protocol::VERSION])],
            &mut SendAncillaryBuffer::default(),
            SendFlags::empty(),
        )
        .map_io_err(|| "Failed to send version.")?;

        let version = unsafe { response!(u8)(&socket, RecvFlags::empty()) }?;
        if version != protocol::VERSION {
            return Err(ClientError::VersionMismatch { actual: version });
        }
    }

    Ok(socket)
}

pub fn add<Server: AsFd, Data: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    to: RingKind,
    mime_type: MimeType,
    data: Data,
) -> Result<AddResponse, ClientError> {
    if FileType::from_raw_mode(
        statx(&data, c"", AtFlags::EMPTY_PATH, StatxFlags::TYPE)
            .map_io_err(|| "Failed to statx file.")?
            .stx_mode
            .into(),
    ) == FileType::RegularFile
    {
        add_unchecked(server, addr, to, mime_type, data)
    } else {
        let file = openat(CWD, c".", OFlags::RDWR | OFlags::TMPFILE, Mode::empty())
            .map_io_err(|| "Failed to create intermediary data file.")?;
        let mut file = File::from(file);

        io::copy(
            &mut *ManuallyDrop::new(unsafe { File::from_raw_fd(data.as_fd().as_raw_fd()) }),
            &mut file,
        )
        .map_io_err(|| "Failed to copy intermediary data file.")?;
        file.seek(SeekFrom::Start(0))
            .map_io_err(|| "Failed to reset intermediary data file offset.")?;

        add_unchecked(server, addr, to, mime_type, &file)
    }
}

pub fn add_unchecked<Server: AsFd, Data: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    to: RingKind,
    mime_type: MimeType,
    data: Data,
) -> Result<AddResponse, ClientError> {
    send_add(&server, addr, to, mime_type, data, SendFlags::empty())?;
    unsafe { recv_add(&server, RecvFlags::empty()) }
}

pub fn send_add<Server: AsFd, Data: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    to: RingKind,
    mime_type: MimeType,
    data: Data,
    flags: SendFlags,
) -> Result<(), ClientError> {
    request_with_fd(&server, addr, Request::Add { to, mime_type }, data, flags)
}

/// This is a low-level method that can be used for high-performance data
/// addition through the use of pipelined ADDs via [`send_add`].
///
/// # Safety
///
/// The previous request must have been an ADD.
pub unsafe fn recv_add<Server: AsFd>(
    server: Server,
    flags: RecvFlags,
) -> Result<AddResponse, ClientError> {
    response!(AddResponse)(&server, flags)
}

pub fn move_to_front<Server: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    id: u64,
    to: Option<RingKind>,
) -> Result<MoveToFrontResponse, ClientError> {
    request(
        &server,
        addr,
        Request::MoveToFront { id, to },
        SendFlags::empty(),
    )?;
    unsafe { response!(MoveToFrontResponse)(&server, RecvFlags::empty()) }
}

pub fn swap<Server: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    id1: u64,
    id2: u64,
) -> Result<SwapResponse, ClientError> {
    request(
        &server,
        addr,
        Request::Swap { id1, id2 },
        SendFlags::empty(),
    )?;
    unsafe { response!(SwapResponse)(&server, RecvFlags::empty()) }
}

pub fn remove<Server: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    id: u64,
) -> Result<RemoveResponse, ClientError> {
    request(&server, addr, Request::Remove { id }, SendFlags::empty())?;
    unsafe { response!(RemoveResponse)(&server, RecvFlags::empty()) }
}

pub fn garbage_collect<Server: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
) -> Result<GarbageCollectResponse, ClientError> {
    request(&server, addr, Request::GarbageCollect, SendFlags::empty())?;
    unsafe { response!(GarbageCollectResponse)(&server, RecvFlags::empty()) }
}

fn request(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    request: Request,
    flags: SendFlags,
) -> Result<(), ClientError> {
    request_with_ancillary(
        server,
        addr,
        request,
        &mut SendAncillaryBuffer::default(),
        flags,
    )
}

fn request_with_fd(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    request: Request,
    fd: impl AsFd,
    flags: SendFlags,
) -> Result<(), ClientError> {
    let mut space = [0; rustix::cmsg_space!(ScmRights(1))];
    let mut buf = SendAncillaryBuffer::new(&mut space);
    let fds = [fd.as_fd()];
    {
        let success = buf.push(SendAncillaryMessage::ScmRights(&fds));
        debug_assert!(success);
    }

    request_with_ancillary(server, addr, request, &mut buf, flags)
}

fn request_with_ancillary(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    request: Request,
    ancillary: &mut SendAncillaryBuffer,
    flags: SendFlags,
) -> Result<(), ClientError> {
    sendmsg_unix(
        server,
        addr,
        &[IoSlice::new(request.as_bytes())],
        ancillary,
        flags,
    )
    .map_io_err(|| format!("Failed to send request: {request:?}"))?;
    Ok(())
}

unsafe fn response<T: Copy, const N: usize>(
    server: impl AsFd,
    flags: RecvFlags,
) -> Result<T, ClientError> {
    let type_name = || {
        let name = std::any::type_name::<T>();
        if let Some((_, name)) = name.rsplit_once(':') {
            name
        } else {
            name
        }
    };

    let mut buf = [0u8; N];
    let result = recvmsg(
        server,
        &mut [IoSliceMut::new(buf.as_mut_slice())],
        &mut RecvAncillaryBuffer::default(),
        RecvFlags::TRUNC | flags,
    )
    .map_io_err(|| format!("Failed to receive {}.", type_name()))?;
    if result.bytes != size_of::<T>() {
        return Err(ClientError::InvalidResponse {
            context: format!("Bad {}.", type_name()).into(),
        });
    }
    Ok(unsafe { *buf.as_ptr().cast::<T>() })
}
