use std::{
    any::TypeId,
    fs::File,
    io,
    io::{IoSlice, IoSliceMut, Seek, SeekFrom},
    mem::ManuallyDrop,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
};

use ringboard_core::{
    protocol,
    protocol::{
        AddResponse, GarbageCollectResponse, MimeType, MoveToFrontResponse, RemoveResponse,
        Request, Response, RingKind, SwapResponse,
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

use crate::ClientError;

macro_rules! response {
    ($t:ty) => {
        /// This is a low-level method that can be used for high-throughput requests
        /// through the use of pipelining via [`Self::send`].
        ///
        /// # Safety
        ///
        /// The received response must have been for a request of this type.
        pub unsafe fn recv<Server: AsFd>(
            server: Server,
            flags: RecvFlags,
        ) -> Result<Response<$t>, ClientError> {
            if TypeId::of::<$t>() == TypeId::of::<VersionResponse>() {
                response::<$t, { size_of::<$t>() }>(&server, flags)
            } else {
                response::<$t, { size_of::<Response<$t>>() }>(&server, flags)
            }
        }
    };
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

        let Response {
            sequence_number: _,
            value: VersionResponse(version),
        } = unsafe {
            response!(VersionResponse);
            recv(&socket, RecvFlags::empty())
        }?;
        if version != protocol::VERSION {
            return Err(ClientError::VersionMismatch { actual: version });
        }
    }

    Ok(socket)
}

#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
struct VersionResponse(u8);

pub struct AddRequest;

impl AddRequest {
    pub fn response<Server: AsFd, Data: AsFd>(
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
            Self::response_add_unchecked(server, addr, to, mime_type, data)
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

            Self::response_add_unchecked(server, addr, to, mime_type, &file)
        }
    }

    pub fn response_add_unchecked<Server: AsFd, Data: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        to: RingKind,
        mime_type: MimeType,
        data: Data,
    ) -> Result<AddResponse, ClientError> {
        Self::send(&server, addr, to, mime_type, data, SendFlags::empty())?;
        unsafe { Self::recv(&server, RecvFlags::empty()) }.map(
            |Response {
                 sequence_number: _,
                 value,
             }| value,
        )
    }

    pub fn send<Server: AsFd, Data: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        to: RingKind,
        mime_type: MimeType,
        data: Data,
        flags: SendFlags,
    ) -> Result<(), ClientError> {
        request_with_fd(&server, addr, Request::Add { to, mime_type }, data, flags)
    }

    response!(AddResponse);
}

pub struct MoveToFrontRequest;

impl MoveToFrontRequest {
    pub fn response<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        id: u64,
        to: Option<RingKind>,
    ) -> Result<MoveToFrontResponse, ClientError> {
        Self::send(&server, addr, id, to, SendFlags::empty())?;
        unsafe { Self::recv(&server, RecvFlags::empty()) }.map(
            |Response {
                 sequence_number: _,
                 value,
             }| value,
        )
    }

    pub fn send<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        id: u64,
        to: Option<RingKind>,
        flags: SendFlags,
    ) -> Result<(), ClientError> {
        request(&server, addr, Request::MoveToFront { id, to }, flags)
    }

    response!(MoveToFrontResponse);
}

pub struct SwapRequest;

impl SwapRequest {
    pub fn response<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        id1: u64,
        id2: u64,
    ) -> Result<SwapResponse, ClientError> {
        Self::send(&server, addr, id1, id2, SendFlags::empty())?;
        unsafe { Self::recv(&server, RecvFlags::empty()) }.map(
            |Response {
                 sequence_number: _,
                 value,
             }| value,
        )
    }

    pub fn send<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        id1: u64,
        id2: u64,
        flags: SendFlags,
    ) -> Result<(), ClientError> {
        request(&server, addr, Request::Swap { id1, id2 }, flags)
    }

    response!(SwapResponse);
}

pub struct RemoveRequest;

impl RemoveRequest {
    pub fn response<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        id: u64,
    ) -> Result<RemoveResponse, ClientError> {
        Self::send(&server, addr, id, SendFlags::empty())?;
        unsafe { Self::recv(&server, RecvFlags::empty()) }.map(
            |Response {
                 sequence_number: _,
                 value,
             }| value,
        )
    }

    pub fn send<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        id: u64,
        flags: SendFlags,
    ) -> Result<(), ClientError> {
        request(&server, addr, Request::Remove { id }, flags)
    }

    response!(RemoveResponse);
}

pub struct GarbageCollectRequest;

impl GarbageCollectRequest {
    pub fn response<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        max_wasted_bytes: u64,
    ) -> Result<GarbageCollectResponse, ClientError> {
        Self::send(&server, addr, max_wasted_bytes, SendFlags::empty())?;
        unsafe { Self::recv(&server, RecvFlags::empty()) }.map(
            |Response {
                 sequence_number: _,
                 value,
             }| value,
        )
    }

    pub fn send<Server: AsFd>(
        server: Server,
        addr: &SocketAddrUnix,
        max_wasted_bytes: u64,
        flags: SendFlags,
    ) -> Result<(), ClientError> {
        request(
            &server,
            addr,
            Request::GarbageCollect { max_wasted_bytes },
            flags,
        )
    }

    response!(GarbageCollectResponse);
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

unsafe fn response<T: Copy + 'static, const N: usize>(
    server: impl AsFd,
    flags: RecvFlags,
) -> Result<Response<T>, ClientError> {
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

    if result.bytes != N {
        return Err(ClientError::InvalidResponse {
            context: format!("Bad {}.", type_name()).into(),
        });
    }
    debug_assert!(!result.flags.contains(RecvFlags::TRUNC));

    if TypeId::of::<T>() == TypeId::of::<VersionResponse>() {
        Ok(Response {
            sequence_number: 0,
            value: *unsafe { &buf.as_ptr().cast::<T>().read_unaligned() },
        })
    } else {
        Ok(*unsafe { &buf.as_ptr().cast::<Response<T>>().read_unaligned() })
    }
}
