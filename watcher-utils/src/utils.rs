use std::{
    io::IoSliceMut,
    os::fd::{AsFd, OwnedFd},
};

use ringboard_sdk::{
    ClientError,
    api::{PASTE_SERVER_PROTOCOL_VERSION, PasteCommand},
    core::IoErr,
};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage::ScmRights, RecvFlags, recvmsg};

pub fn read_paste_command(
    paste_socket: impl AsFd,
    ancillary_buf: &mut [u8; rustix::cmsg_space!(ScmRights(1))],
) -> Result<(PasteCommand, Option<OwnedFd>), ClientError> {
    let mut buf = [0; size_of::<PasteCommand>()];
    let mut ancillary = RecvAncillaryBuffer::new(ancillary_buf);
    let msg = recvmsg(
        paste_socket,
        &mut [IoSliceMut::new(&mut buf)],
        &mut ancillary,
        RecvFlags::TRUNC,
    )
    .map_io_err(|| "Failed to recv client msg.")?;
    let version = buf[0];
    if version != PASTE_SERVER_PROTOCOL_VERSION {
        return Err(ClientError::VersionMismatch {
            expected: PASTE_SERVER_PROTOCOL_VERSION,
            actual: version,
        });
    }
    if msg.bytes != buf.len() {
        return Err(ClientError::InvalidResponse {
            context: "Bad paste command.".into(),
        });
    }
    debug_assert!(!msg.flags.contains(RecvFlags::TRUNC));

    let mut data = None;
    for msg in ancillary.drain() {
        if let ScmRights(received_fds) = msg {
            for fd in received_fds {
                debug_assert!(data.is_none());
                data = Some(fd);
            }
        }
    }

    Ok((
        *unsafe { &buf.as_ptr().cast::<PasteCommand>().read_unaligned() },
        data,
    ))
}
