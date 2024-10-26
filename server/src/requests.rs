use std::fmt::Debug;

use arrayvec::ArrayVec;
use log::{debug, info, warn};
use ringboard_core::{
    AsBytes, protocol,
    protocol::{AddResponse, MimeType, Request, RingKind},
};
use rustix::net::{AncillaryDrain, RecvAncillaryMessage};

use crate::{
    CliError,
    allocator::Allocator,
    send_msg_bufs::{PendingBufAllocation, SendMsgBufs},
};

pub fn connect(payload: &[u8], send_bufs: &mut SendMsgBufs) -> (bool, PendingBufAllocation) {
    debug!("Establishing client/server protocol connection.");
    let version = payload[0];
    let valid = version == protocol::VERSION;
    if !valid {
        warn!(
            "Protocol version mismatch: expected {} but got {version}.",
            protocol::VERSION
        );
    }

    let response = send_bufs.init_buf(
        |_| (),
        |buf| {
            buf.push(protocol::VERSION);
        },
    );

    (valid, response)
}

pub fn handle(
    request_data: &[u8],
    control_data: &mut [u8],
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
    sequence_number: &mut u64,
) -> Result<Option<PendingBufAllocation>, CliError> {
    if request_data.len() < size_of::<Request>() {
        warn!("Dropping invalid request (too short).");
        return Ok(None);
    }
    let request = unsafe { &request_data.as_ptr().cast::<Request>().read_unaligned() };

    macro_rules! reply {
        ($response:expr) => {{ Ok(Some(reply(send_bufs, *sequence_number, $response))) }};
    }

    info!("Processing request: {request:?}");
    *sequence_number = sequence_number.wrapping_add(1);
    match *request {
        Request::Add { to, ref mime_type } => {
            reply!(add(control_data, allocator, to, mime_type)?)
        }
        Request::MoveToFront { id, to } => {
            reply!([allocator.move_to_front(id, to)?])
        }
        Request::Swap { id1, id2 } => reply!([allocator.swap(id1, id2)?]),
        Request::Remove { id } => reply!([allocator.remove(id)?]),
        Request::GarbageCollect { max_wasted_bytes } => {
            reply!([allocator.gc(max_wasted_bytes)?])
        }
    }
}

fn reply<R: AsBytes + Debug>(
    send_bufs: &mut SendMsgBufs,
    sequence_number: u64,
    responses: impl IntoIterator<Item = R, IntoIter: ExactSizeIterator<Item = R>>,
) -> PendingBufAllocation {
    send_bufs.init_buf(
        |_| (),
        |buf| {
            let responses = responses.into_iter();
            debug_assert_eq!(responses.len(), 1);
            for response in responses {
                info!("Replying: {sequence_number}@{response:?}");
                buf.extend_from_slice(&sequence_number.to_ne_bytes());
                buf.extend_from_slice(response.as_bytes());
            }
        },
    )
}

fn add(
    control_data: &mut [u8],
    allocator: &mut Allocator,
    kind: RingKind,
    mime_type: &MimeType,
) -> Result<impl ExactSizeIterator<Item = AddResponse>, CliError> {
    let mut responses = ArrayVec::<_, 1>::new();

    for message in unsafe { AncillaryDrain::parse(control_data) } {
        if let RecvAncillaryMessage::ScmRights(received_fds) = message {
            for fd in received_fds {
                responses.push(allocator.add(fd, kind, mime_type)?);
            }
        }
    }

    Ok(responses.into_iter())
}
