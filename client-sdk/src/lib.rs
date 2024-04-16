#![allow(clippy::missing_errors_doc)]

use std::{
    borrow::Cow,
    io::{IoSlice, IoSliceMut},
    mem,
    os::fd::{AsFd, OwnedFd},
};

use ringboard_core::{
    protocol,
    protocol::{
        AddResponse, GarbageCollectResponse, MimeType, MoveToFrontResponse, RemoveResponse,
        Request, RingKind, SwapResponse,
    },
    AsBytes, IoErr,
};
use rustix::net::{
    connect_unix, recvmsg, sendmsg_unix, socket, AddressFamily, RecvAncillaryBuffer, RecvFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketType,
};
use thiserror::Error;

macro_rules! response {
    ($t:ty) => {
        response::<$t, { mem::size_of::<$t>() }>
    };
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Core(#[from] ringboard_core::Error),
    #[error(
        "Protocol version mismatch: expected {} but got {actual}",
        protocol::VERSION
    )]
    VersionMismatch { actual: u8 },
    #[error("The server returned an invalid response.")]
    InvalidResponse { context: Cow<'static, str> },
}

pub fn connect_to_server(addr: &SocketAddrUnix) -> Result<OwnedFd, Error> {
    let socket = socket(AddressFamily::UNIX, SocketType::SEQPACKET, None)
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
            return Err(Error::VersionMismatch { actual: version });
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
) -> Result<AddResponse, Error> {
    // TODO figure out if we need to make a copy of the file for X11/Wayland.
    //  As in can you copy from stdin to clipboard and will that be routed directly
    //  to the server.
    add_send(&server, addr, to, mime_type, data, SendFlags::empty())?;
    unsafe { add_recv(&server, RecvFlags::empty()) }
}

pub fn add_send<Server: AsFd, Data: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    to: RingKind,
    mime_type: MimeType,
    data: Data,
    flags: SendFlags,
) -> Result<(), Error> {
    request_with_fd(&server, addr, Request::Add { to, mime_type }, data, flags)
}

pub unsafe fn add_recv<Server: AsFd>(
    server: Server,
    flags: RecvFlags,
) -> Result<AddResponse, Error> {
    response!(AddResponse)(&server, flags)
}

pub fn move_to_front<Server: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
    id: u64,
    to: Option<RingKind>,
) -> Result<MoveToFrontResponse, Error> {
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
) -> Result<SwapResponse, Error> {
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
) -> Result<RemoveResponse, Error> {
    request(&server, addr, Request::Remove { id }, SendFlags::empty())?;
    unsafe { response!(RemoveResponse)(&server, RecvFlags::empty()) }
}

pub fn garbage_collect<Server: AsFd>(
    server: Server,
    addr: &SocketAddrUnix,
) -> Result<GarbageCollectResponse, Error> {
    request(&server, addr, Request::GarbageCollect, SendFlags::empty())?;
    unsafe { response!(GarbageCollectResponse)(&server, RecvFlags::empty()) }
}

fn request(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    request: Request,
    flags: SendFlags,
) -> Result<(), Error> {
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
) -> Result<(), Error> {
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
) -> Result<(), Error> {
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
) -> Result<T, Error> {
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
    if result.bytes != mem::size_of::<T>() {
        return Err(Error::InvalidResponse {
            context: format!("Bad {}.", type_name()).into(),
        });
    }
    Ok(unsafe { *buf.as_ptr().cast::<T>() })
}
