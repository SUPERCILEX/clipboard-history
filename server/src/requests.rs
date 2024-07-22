use std::fmt::Debug;

use arrayvec::ArrayVec;
use log::{info, warn};
use ringboard_core::{
    protocol,
    protocol::{MimeType, Request, RingKind},
    AsBytes,
};
use rustix::net::{AncillaryDrain, RecvAncillaryMessage};

use crate::{
    allocator::Allocator,
    send_msg_bufs::{SendBufAllocation, SendMsgBufs},
    CliError,
};

pub fn connect(
    payload: &[u8],
    send_bufs: &mut SendMsgBufs,
) -> Result<(bool, SendBufAllocation), CliError> {
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
            |_| (),
            |buf| {
                buf.push(protocol::VERSION);
            },
        )
        .map_err(|()| CliError::Internal {
            context: "Didn't allocate enough send buffers.".into(),
        })?;

    Ok((valid, response))
}

pub fn handle(
    request_data: &[u8],
    control_data: &mut [u8],
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
) -> Result<Option<SendBufAllocation>, CliError> {
    if request_data.len() < size_of::<Request>() {
        warn!("Dropping invalid request (too short).");
        return Ok(None);
    }
    let request = unsafe { &request_data.as_ptr().cast::<Request>().read_unaligned() };

    info!("Processing request: {request:?}");
    match *request {
        Request::Add { to, ref mime_type } => {
            add(control_data, send_bufs, allocator, to, mime_type).map(Some)
        }
        Request::MoveToFront { id, to } => {
            reply(send_bufs, [allocator.move_to_front(id, to)?]).map(Some)
        }
        Request::Swap { id1, id2 } => reply(send_bufs, [allocator.swap(id1, id2)?]).map(Some),
        Request::Remove { id } => reply(send_bufs, [allocator.remove(id)?]).map(Some),
        Request::GarbageCollect { max_wasted_bytes } => {
            reply(send_bufs, [allocator.gc(max_wasted_bytes)?]).map(Some)
        }
    }
}

fn reply<R: AsBytes + Debug>(
    send_bufs: &mut SendMsgBufs,
    responses: impl IntoIterator<Item = R>,
) -> Result<SendBufAllocation, CliError> {
    send_bufs
        .alloc(
            |_| (),
            |buf| {
                for response in responses {
                    info!("Replying: {response:?}");
                    buf.extend_from_slice(response.as_bytes());
                }
            },
        )
        .map_err(|()| CliError::Internal {
            context: "Didn't allocate enough send buffers.".into(),
        })
}

fn add(
    control_data: &mut [u8],
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
    kind: RingKind,
    mime_type: &MimeType,
) -> Result<SendBufAllocation, CliError> {
    let mut responses = ArrayVec::<_, 1>::new();

    for message in unsafe { AncillaryDrain::parse(control_data) } {
        if let RecvAncillaryMessage::ScmRights(received_fds) = message {
            for fd in received_fds {
                responses.push(allocator.add(fd, kind, mime_type)?);
            }
        }
    }

    reply(send_bufs, responses)
}
