use arrayvec::ArrayVec;
use clipboard_history_core::{protocol, protocol::Request};
use log::{info, warn};
use rustix::net::{AncillaryDrain, RecvAncillaryMessage};

use crate::{
    allocator::{Allocator, RingKind},
    send_msg_bufs::{SendMsgBufs, Token},
    CliError,
};

pub fn connect(
    payload: &[u8],
    send_bufs: &mut SendMsgBufs,
) -> Result<(bool, (Token, *const libc::msghdr)), CliError> {
    info!("Establishing client/server protocol connection.");
    let version = payload[0];
    let valid = version == protocol::VERSION;
    if !valid {
        warn!(
            "Protocol version mismatch: expected {} but got {version}.",
            protocol::VERSION
        );
    }

    let response = send_bufs
        .alloc(
            0,
            1,
            |_| (),
            |buf| {
                buf[0].write(protocol::VERSION);
            },
        )
        .map_err(|()| CliError::Internal {
            context: "Didn't allocate enough send buffers.".into(),
        })?;

    Ok((valid, response))
}

pub fn handle(
    request: &Request,
    control_data: &mut [u8],
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
) -> Result<Option<(Token, *const libc::msghdr)>, CliError> {
    info!("Processing request: {request:?}");
    match request {
        Request::Add => {
            let mut fds = ArrayVec::<_, 1>::new();

            for message in unsafe { AncillaryDrain::parse(control_data) } {
                if let RecvAncillaryMessage::ScmRights(received_fds) = message {
                    fds.extend(received_fds);
                }
            }

            let mut ids = ArrayVec::<_, 1>::new();
            for fd in fds {
                let id = allocator.add(fd, RingKind::Main, "")?;
                info!("Entry added: {id}");
                ids.push(id);
            }

            Ok(None)
        }
    }
}
