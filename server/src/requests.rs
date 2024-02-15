use std::mem;

use arrayvec::ArrayVec;
use clipboard_history_core::{
    protocol,
    protocol::{composite_id, AddResponse, MimeType, Request, RingKind},
    AsBytes,
};
use log::{info, warn};
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
    if request_data.len() < mem::size_of::<Request>() {
        warn!("Dropping invalid request (too short).");
        return Ok(None);
    }
    let request = unsafe { &*request_data.as_ptr().cast::<Request>() };

    info!("Processing request: {request:?}");
    match request {
        &Request::Add { to, ref mime_type } => {
            add(control_data, send_bufs, allocator, to, mime_type).map(Some)
        }
        &Request::MoveToFront { id, to } => move_to_front(send_bufs, allocator, id, to).map(Some),
        &Request::Swap { id1, id2 } => swap(send_bufs, allocator, id1, id2).map(Some),
        &Request::Remove { id } => remove(send_bufs, allocator, id).map(Some),
        Request::ReloadSettings => reload_settings(control_data, send_bufs, allocator).map(Some),
        Request::GarbageCollect => gc(allocator).map(|()| None),
    }
}

fn add(
    control_data: &mut [u8],
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
    kind: RingKind,
    mime_type: &MimeType,
) -> Result<SendBufAllocation, CliError> {
    let mut ids = ArrayVec::<_, 1>::new();

    for message in unsafe { AncillaryDrain::parse(control_data) } {
        if let RecvAncillaryMessage::ScmRights(received_fds) = message {
            for fd in received_fds {
                let id = composite_id(kind, allocator.add(fd, kind, mime_type)?);
                info!("Entry added: {id}");
                ids.push(id);
            }
        }
    }

    send_bufs
        .alloc(
            |_| (),
            |buf| {
                for id in ids {
                    buf.extend_from_slice(AddResponse { id }.as_bytes());
                }
            },
        )
        .map_err(|()| CliError::Internal {
            context: "Didn't allocate enough send buffers.".into(),
        })
}

fn move_to_front(
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
    id: u64,
    to: Option<RingKind>,
) -> Result<SendBufAllocation, CliError> {
    todo!()
}

fn swap(
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
    id1: u64,
    id2: u64,
) -> Result<SendBufAllocation, CliError> {
    todo!()
}

fn remove(
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
    id: u64,
) -> Result<SendBufAllocation, CliError> {
    todo!()
}

fn reload_settings(
    control_data: &mut [u8],
    send_bufs: &mut SendMsgBufs,
    allocator: &mut Allocator,
) -> Result<SendBufAllocation, CliError> {
    todo!()
}

fn gc(allocator: &mut Allocator) -> Result<(), CliError> {
    todo!()
}
