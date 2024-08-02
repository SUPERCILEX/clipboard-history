#![feature(let_chains)]

use std::{
    fs::File,
    io::IoSliceMut,
    mem,
    os::{fd::AsFd, unix::fs::FileExt},
    slice,
};

use arrayvec::ArrayVec;
use error_stack::Report;
use log::{debug, error, info, trace, warn};
use ringboard_sdk::{
    api::{connect_to_server, AddRequest, MoveToFrontRequest},
    core::{
        dirs::{paste_socket_file, socket_file},
        init_unix_server,
        protocol::{AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, RingKind},
        ring::Mmap,
        Error, IoErr,
    },
};
use rustix::{
    event::epoll,
    fs::{memfd_create, openat, MemfdFlags, Mode, OFlags, CWD},
    net::{
        recvmsg, RecvAncillaryBuffer, RecvAncillaryMessage::ScmRights, RecvFlags, SocketAddrUnix,
        SocketType,
    },
    path::Arg,
};
use thiserror::Error;
use x11rb::{
    atom_manager,
    connection::{Connection, RequestConnection},
    cookie::Cookie,
    errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError},
    protocol::{
        xfixes,
        xfixes::{select_selection_input, SelectionEventMask},
        xproto::{
            Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, GetAtomNameReply,
            GetPropertyType, PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent,
            Window, WindowClass, SELECTION_NOTIFY_EVENT,
        },
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as WrapperConnExt,
    x11_utils::X11Error,
};

use crate::{
    best_target::BestMimeTypeFinder,
    deduplication::{CopyData, CopyDeduplication},
};

mod best_target;
mod deduplication;

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] Error),
    #[error("{0}")]
    Sdk(#[from] ringboard_sdk::ClientError),
    #[error("failed to connect to X11 server")]
    X11Connect(#[from] ConnectError),
    #[error("X11 request failed")]
    X11Connection(#[from] ConnectionError),
    #[error("X11 reply failed")]
    X11Error(X11Error),
    #[error("failed to create X11 ID")]
    X11IdsExhausted,
    #[error("unsupported X11 version: XFixes extension not available")]
    X11NoXfixes,
}

impl From<X11Error> for CliError {
    fn from(value: X11Error) -> Self {
        Self::X11Error(value)
    }
}

impl From<ReplyError> for CliError {
    fn from(value: ReplyError) -> Self {
        match value {
            ReplyError::ConnectionError(e) => e.into(),
            ReplyError::X11Error(e) => e.into(),
        }
    }
}

impl From<ReplyOrIdError> for CliError {
    fn from(value: ReplyOrIdError) -> Self {
        match value {
            ReplyOrIdError::IdsExhausted => Self::X11IdsExhausted,
            ReplyOrIdError::ConnectionError(e) => e.into(),
            ReplyOrIdError::X11Error(e) => e.into(),
        }
    }
}

impl From<IdNotFoundError> for CliError {
    fn from(value: IdNotFoundError) -> Self {
        ringboard_sdk::ClientError::from(value).into()
    }
}

#[derive(Error, Debug)]
enum Wrapper {
    #[error("{0}")]
    W(String),
}

fn main() -> error_stack::Result<(), Wrapper> {
    #[cfg(not(debug_assertions))]
    error_stack::Report::install_debug_hook::<std::panic::Location>(|_, _| {});

    if cfg!(debug_assertions) {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    } else {
        env_logger::init();
    }

    run().map_err(into_report)
}

fn into_report(cli_err: CliError) -> Report<Wrapper> {
    let wrapper = Wrapper::W(cli_err.to_string());
    match cli_err {
        CliError::Core(e) => e.into_report(wrapper),
        CliError::Sdk(e) => e.into_report(wrapper),
        CliError::X11Connect(e) => Report::new(e).change_context(wrapper),
        CliError::X11Connection(e) => Report::new(e).change_context(wrapper),
        CliError::X11Error(e) => Report::new(wrapper).attach_printable(format!("{e:?}")),
        CliError::X11IdsExhausted | CliError::X11NoXfixes => Report::new(wrapper),
    }
}

#[derive(Default, Debug)]
enum State {
    #[default]
    Free,
    FastPathPendingSelection {
        selection: Atom,
    },
    TargetsRequest {
        selection: Atom,
        allow_plain_text: bool,
    },
    PendingSelection {
        mime_atom: Atom,
        mime_type: MimeType,
    },
    PendingIncr {
        mime_atom: Atom,
        mime_type: MimeType,
        file: Option<File>,
        written: u64,
    },
}

const MAX_CONCURRENT_TRANSFERS: usize = 4;
const BASE_TRANSFER_ATOM: AtomEnum = AtomEnum::CUT_BUFFE_R0;

#[derive(Default)]
struct TransferAtomAllocator {
    windows: [Window; MAX_CONCURRENT_TRANSFERS],
    states: [State; MAX_CONCURRENT_TRANSFERS],
    next: u8,
}

impl TransferAtomAllocator {
    fn alloc(&mut self) -> (&mut State, Window, Atom) {
        const _: () = assert!(MAX_CONCURRENT_TRANSFERS.is_power_of_two());

        let next = usize::from(self.next);

        if !matches!(self.states[next], State::Free) {
            warn!("Too many ongoing transfers, dropping old transfer.");
        }
        let state = &mut self.states[next];
        let transfer_window = self.windows[next];
        let transfer_atom = Self::transfer_atom(next);

        self.next = u8::try_from((next + 1) & (MAX_CONCURRENT_TRANSFERS - 1)).unwrap();
        (state, transfer_window, transfer_atom)
    }

    fn get(&mut self, window: Window) -> Option<(&mut State, Atom)> {
        self.windows
            .iter()
            .position(|&id| id == window)
            .map(|i| (&mut self.states[i], Self::transfer_atom(i)))
    }

    fn transfer_atom(id: usize) -> Atom {
        Atom::from(BASE_TRANSFER_ATOM) + Atom::try_from(id).unwrap()
    }
}

atom_manager! {
    Atoms:
    AtomsCookie {
        _NET_WM_NAME,
        UTF8_STRING,

        CLIPBOARD,
        TARGETS,
        INCR,

        TEXT,
        STRING,
        text_plain: b"text/plain",
        text_plain_utf8: b"text/plain;charset=utf-8",
        text_plain_us_ascii: b"text/plain;charset=us-ascii",
        text_plain_unicode: b"text/plain;charset=unicode",
    }
}

#[derive(Copy, Clone, Debug)]
struct PasteAtom {
    atom: Atom,
    is_text: bool,
}

fn run() -> Result<(), CliError> {
    info!(
        "Starting Ringboard X11 clipboard listener v{}.",
        env!("CARGO_PKG_VERSION")
    );

    let server = {
        let socket_file = socket_file();
        let addr = SocketAddrUnix::new(&socket_file)
            .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?;
        connect_to_server(&addr)?
    };
    debug!("Ringboard connection established.");

    let (conn, root) = {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;

        (conn, root)
    };
    debug!("X11 connection established.");

    conn.prefetch_extension_information(xfixes::X11_EXTENSION_NAME)?;
    let atoms @ Atoms {
        _NET_WM_NAME: window_name_atom,
        UTF8_STRING: utf8_string_atom,
        CLIPBOARD: clipboard_atom,
        ..
    } = Atoms::new(&conn)?.reply()?;
    debug!("Atom internment complete.");

    let create_window = |title: &[u8], aux, kind| -> Result<_, CliError> {
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            kind,
            x11rb::COPY_FROM_PARENT,
            &aux,
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            window_name_atom,
            utf8_string_atom,
            title,
        )?;
        Ok(window)
    };
    let mut transfer_windows = ArrayVec::<_, MAX_CONCURRENT_TRANSFERS>::new_const();
    for i in 0..MAX_CONCURRENT_TRANSFERS {
        transfer_windows.push(create_window(
            format!("Ringboard data transfer {}", i + 1).as_bytes(),
            CreateWindowAux::default().event_mask(EventMask::PROPERTY_CHANGE),
            WindowClass::INPUT_ONLY,
        )?);
    }
    let paste_window = create_window(
        b"Ringboard paste",
        CreateWindowAux::default(),
        WindowClass::INPUT_OUTPUT,
    )?;
    debug!("Created utility windows.");

    conn.extension_information(xfixes::X11_EXTENSION_NAME)?
        .ok_or(CliError::X11NoXfixes)?;
    xfixes::query_version(&conn, 5, 0)?.reply()?;
    debug!("XFixes extension availability confirmed.");

    select_selection_input(
        &conn,
        root,
        clipboard_atom,
        SelectionEventMask::SET_SELECTION_OWNER,
    )?;
    debug!("Selection owner listener registered.");

    let paste_socket = init_unix_server(paste_socket_file(), SocketType::DGRAM)?;
    debug!("Initialized paste server");

    let mut ancillary_buf = [0; rustix::cmsg_space!(ScmRights(1))];
    let mut last_paste = None;

    let epoll =
        epoll::create(epoll::CreateFlags::empty()).map_io_err(|| "Failed to create epoll.")?;
    epoll::add(
        &epoll,
        conn.stream(),
        epoll::EventData::new_u64(0),
        epoll::EventFlags::IN,
    )
    .map_io_err(|| "Failed to register X11 server with epoll.")?;
    epoll::add(
        &epoll,
        &paste_socket,
        epoll::EventData::new_u64(1),
        epoll::EventFlags::IN,
    )
    .map_io_err(|| "Failed to register paste server with epoll.")?;
    let mut epoll_events = epoll::EventVec::with_capacity(2);

    let mut allocator = TransferAtomAllocator {
        windows: transfer_windows.into_inner().unwrap(),
        states: [const { State::Free }; MAX_CONCURRENT_TRANSFERS],
        next: 0,
    };

    let mut deduplicator = CopyDeduplication::new()?;

    info!("Starting event loop.");
    conn.flush()?;
    loop {
        while let Some(event) = conn.poll_for_event()? {
            handle_x11_event(
                event,
                &conn,
                &atoms,
                &mut allocator,
                &server,
                &mut deduplicator,
                paste_window,
                &mut last_paste,
            )?;
        }

        trace!("Waiting for event.");
        epoll::wait(&epoll, &mut epoll_events, -1)
            .map_io_err(|| "Failed to wait for epoll events.")?;

        for epoll::Event { flags: _, data } in &epoll_events {
            match data.u64() {
                0 => continue,
                1 => handle_paste_event(
                    &conn,
                    clipboard_atom,
                    paste_window,
                    &paste_socket,
                    &mut ancillary_buf,
                    &mut last_paste,
                )?,
                _ => unreachable!(),
            }
        }
    }
}

fn handle_x11_event(
    event: Event,
    conn: &RustConnection,
    atoms: &Atoms,
    allocator: &mut TransferAtomAllocator,
    server: impl AsFd,
    deduplicator: &mut CopyDeduplication,
    paste_window: Window,
    last_paste: &mut Option<(Mmap, PasteAtom)>,
) -> Result<(), CliError> {
    let &Atoms {
        _NET_WM_NAME: window_name_atom,
        UTF8_STRING: utf8_string_atom,
        CLIPBOARD: clipboard_atom,
        TARGETS: targets_atom,
        INCR: incr_atom,
        ..
    } = atoms;
    let mut pending_atom_cookies = ArrayVec::<(Cookie<_, GetAtomNameReply>, _), 8>::new_const();

    match event {
        Event::SelectionRequest(SelectionRequestEvent {
            response_type: _,
            sequence,
            time,
            owner: _,
            requestor,
            selection,
            target,
            property,
        }) => {
            if cfg!(debug_assertions) {
                debug!(
                    "Paste request received for target {}",
                    conn.get_atom_name(target)?.reply()?.name.to_string_lossy()
                );
            }
            let reply = |property| {
                conn.send_event(
                    false,
                    requestor,
                    EventMask::NO_EVENT,
                    SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence,
                        time,
                        requestor,
                        selection,
                        target,
                        property,
                    },
                )?
                .check()?;
                Ok(())
            };

            if selection != clipboard_atom {
                debug!("Unsupported selection type.");
                return reply(x11rb::NONE);
            }
            let Some((ref paste_file, PasteAtom { atom, is_text })) = *last_paste else {
                debug!("Nothing to paste.");
                return reply(x11rb::NONE);
            };

            let mut supported_atoms = ArrayVec::<_, 9>::new_const();
            supported_atoms.push(targets_atom);
            if atom != x11rb::NONE {
                supported_atoms.push(atom);
            }
            if is_text {
                supported_atoms
                    .try_extend_from_slice(&[
                        utf8_string_atom,
                        atoms.TEXT,
                        atoms.STRING,
                        atoms.text_plain,
                        atoms.text_plain_utf8,
                        atoms.text_plain_us_ascii,
                        atoms.text_plain_unicode,
                    ])
                    .unwrap();
            }
            if !supported_atoms.contains(&target) {
                debug!("Unsupported target.");
                return reply(x11rb::NONE);
            }

            let property = if property == x11rb::NONE {
                debug!("Obsolete client detected.");
                target
            } else {
                property
            };

            if target == targets_atom {
                debug!("Responding to paste request with TARGETS.");
                conn.change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &supported_atoms,
                )?;
                reply(property)?;
                return Ok(());
            }

            if paste_file.len() > (1 << 20) {
                debug!("Starting paste request INCR transfer.");
                // TODO
            } else {
                conn.change_property8(PropMode::REPLACE, requestor, property, target, paste_file)?;
                reply(property)?;
                info!("Responded to paste request with small selection.");
            }
        }
        Event::SelectionNotify(event) if event.requestor == paste_window => {
            error!("Trying to paste into ourselves!");
        }
        Event::SelectionClear(event) => {
            if event.owner != paste_window && last_paste.take().is_some() {
                info!("Lost selection ownership.");
            }
        }

        Event::XfixesSelectionNotify(event) => {
            if event.owner == paste_window {
                debug!("Ignoring selection notification from ourselves.");
                return Ok(());
            }

            info!("Selection notification received.");
            let (state, transfer_window, transfer_atom) = allocator.alloc();
            *state = State::FastPathPendingSelection {
                selection: event.selection,
            };

            conn.convert_selection(
                transfer_window,
                event.selection,
                utf8_string_atom,
                transfer_atom,
                x11rb::CURRENT_TIME,
            )?
            .check()?;
        }
        Event::SelectionNotify(event) => {
            let Some((state, transfer_atom)) = allocator.get(event.requestor) else {
                warn!(
                    "Ignoring selection notification to unknown requester {}.",
                    event.requestor
                );
                return Ok(());
            };
            debug!("Stage 2 selection notification received.");

            match state {
                &mut State::FastPathPendingSelection { selection } => {
                    if event.property == x11rb::NONE {
                        debug!("UTF8_STRING target fast path failed. Retrying with target query.");
                        *state = State::TargetsRequest {
                            selection,
                            allow_plain_text: true,
                        };
                        conn.convert_selection(
                            event.requestor,
                            selection,
                            targets_atom,
                            transfer_atom,
                            x11rb::CURRENT_TIME,
                        )?
                        .check()?;
                    }
                }
                State::TargetsRequest { .. } => {
                    if event.property == x11rb::NONE {
                        warn!("Targets response cancelled.");
                        *state = State::default();
                    }
                }
                State::PendingSelection { .. } => {
                    if event.property == x11rb::NONE {
                        warn!("Selection transfer cancelled.");
                        *state = State::default();
                    }
                }
                State::Free | State::PendingIncr { .. } => {
                    // Nothing to do
                }
            }
        }
        Event::PropertyNotify(event) => {
            if event.atom == window_name_atom {
                trace!("Ignoring window name property change.");
                return Ok(());
            }
            if event.state != Property::NEW_VALUE {
                trace!(
                    "Ignoring uninteresting property state change: {:?}.",
                    event.state
                );
                return Ok(());
            }
            let property = conn.get_property(
                true,
                event.window,
                event.atom,
                GetPropertyType::ANY,
                0,
                u32::MAX,
            )?;
            conn.flush()?;
            let Some((state, transfer_atom)) = allocator.get(event.window) else {
                warn!(
                    "Ignoring property notify to unknown requester {}.",
                    event.window
                );
                return Ok(());
            };

            match mem::take(state) {
                State::TargetsRequest {
                    selection,
                    allow_plain_text,
                } => {
                    let property = property.reply()?;
                    if property.type_ == incr_atom {
                        warn!("Ignoring abusive TARGETS property.");
                        return Ok(());
                    }

                    let Some(mut value) = property.value32() else {
                        error!("Invalid TARGETS property value format.");
                        return Ok(());
                    };

                    let mut finder = BestMimeTypeFinder::default();
                    if !allow_plain_text {
                        debug!(
                            "Blocking plain text as it returned a blank or empty result on the \
                             fast path."
                        );
                        finder.block_text();
                    }
                    loop {
                        let atom = value.next();
                        if pending_atom_cookies.is_full() || atom.is_none() {
                            for (cookie, atom) in pending_atom_cookies.drain(..) {
                                let reply = cookie.reply()?;
                                let name = reply.name.to_string_lossy();
                                trace!("Target {name:?} available on atom {atom}.");

                                let Ok(mime) = MimeType::from(&name) else {
                                    warn!("Target {name:?} name too long, ignoring.");
                                    continue;
                                };

                                finder.add_mime(&mime, atom);
                            }
                        }
                        let Some(atom) = atom else {
                            break;
                        };
                        pending_atom_cookies.push((conn.get_atom_name(atom)?, atom));
                    }

                    let Some((target, target_mime)) = finder.best() else {
                        warn!("No usable targets returned, dropping selection.");
                        return Ok(());
                    };
                    info!("Choosing target {target_mime:?} on atom {target}.",);

                    *state = State::PendingSelection {
                        mime_atom: target,
                        mime_type: target_mime,
                    };
                    conn.convert_selection(
                        event.window,
                        selection,
                        target,
                        transfer_atom,
                        x11rb::CURRENT_TIME,
                    )?
                    .check()?;
                }
                s @ (State::FastPathPendingSelection { .. } | State::PendingSelection { .. }) => {
                    let (mime_atom, mime_type, fast_path) = match s {
                        State::FastPathPendingSelection { selection } => {
                            (utf8_string_atom, MimeType::new_const(), Some(selection))
                        }
                        State::PendingSelection {
                            mime_atom,
                            mime_type,
                        } => (mime_atom, mime_type, None),
                        _ => unreachable!(),
                    };

                    let property = property.reply()?;
                    if property.type_ == incr_atom {
                        debug!("Waiting for INCR transfer.");
                        *state = State::PendingIncr {
                            mime_atom,
                            mime_type,
                            file: None,
                            written: 0,
                        };
                    } else {
                        if property.value.is_empty()
                            || property.value.iter().all(u8::is_ascii_whitespace)
                        {
                            if let Some(selection) = fast_path {
                                debug!(
                                    "UTF8_STRING target fast path empty or blank. Retrying with \
                                     target query."
                                );
                                *state = State::TargetsRequest {
                                    selection,
                                    allow_plain_text: false,
                                };
                                conn.convert_selection(
                                    event.window,
                                    selection,
                                    targets_atom,
                                    transfer_atom,
                                    x11rb::CURRENT_TIME,
                                )?
                                .check()?;
                            } else {
                                warn!("Dropping empty or blank selection.");
                            }
                            return Ok(());
                        }

                        let data_hash = CopyDeduplication::hash(
                            CopyData::Slice(&property.value),
                            u64::try_from(property.value.len()).unwrap(),
                        );
                        if let Some(existing) =
                            deduplicator.check(data_hash, CopyData::Slice(&property.value))
                        {
                            info!("Promoting duplicate small selection to front.");
                            if let MoveToFrontResponse::Success { id } =
                                MoveToFrontRequest::response(&server, existing, None)?
                            {
                                deduplicator.remember(data_hash, id);
                                return Ok(());
                            }
                        }

                        let file = File::from(
                            memfd_create(c"ringboard_x11_selection", MemfdFlags::empty())
                                .map_io_err(|| "Failed to create selection transfer temp file.")?,
                        );
                        file.write_all_at(&property.value, 0)
                            .map_io_err(|| "Failed to write data to temp file.")?;

                        let AddResponse::Success { id } = AddRequest::response_add_unchecked(
                            &server,
                            RingKind::Main,
                            mime_type,
                            file,
                        )?;
                        deduplicator.remember(data_hash, id);
                        info!("Small selection transfer complete.");
                    }
                }
                State::PendingIncr {
                    mime_atom,
                    mime_type,
                    file,
                    written,
                } => {
                    let file = if let Some(file) = file {
                        file
                    } else {
                        File::from(
                            openat(CWD, c".", OFlags::RDWR | OFlags::TMPFILE, Mode::empty())
                                .map_io_err(|| "Failed to create selection transfer temp file.")?,
                        )
                    };

                    let property = property.reply()?;
                    if property.value.is_empty() {
                        if written == 0 {
                            warn!("Dropping empty INCR selection.");
                            return Ok(());
                        }

                        let data_hash = CopyDeduplication::hash(CopyData::File(&file), written);
                        if let Some(existing) = deduplicator.check(data_hash, CopyData::File(&file))
                        {
                            info!("Promoting duplicate large selection to front.");
                            if let MoveToFrontResponse::Success { id } =
                                MoveToFrontRequest::response(&server, existing, None)?
                            {
                                deduplicator.remember(data_hash, id);
                                return Ok(());
                            }
                        }

                        let AddResponse::Success { id } = AddRequest::response_add_unchecked(
                            &server,
                            RingKind::Main,
                            mime_type,
                            file,
                        )?;
                        deduplicator.remember(data_hash, id);
                        info!("Large selection transfer complete.");
                    } else {
                        debug!("Writing {} bytes for INCR transfer.", property.value.len());
                        file.write_all_at(&property.value, written)
                            .map_io_err(|| "Failed to write data to temp file.")?;
                        *state = State::PendingIncr {
                            mime_atom,
                            mime_type,
                            file: Some(file),
                            written: written + u64::try_from(property.value.len()).unwrap(),
                        }
                    }
                }
                State::Free => {
                    error!(
                        "Received property notification for free atom {}.",
                        conn.get_atom_name(event.atom)?
                            .reply()?
                            .name
                            .to_string_lossy()
                    );
                }
            }
        }
        Event::Error(e) => return Err(e.into()),
        event => {
            debug!("Ignoring unknown X11 event: {event:?}");
        }
    }
    Ok(())
}

fn handle_paste_event(
    conn: &RustConnection,
    clipboard_atom: Atom,
    paste_window: Window,
    paste_socket: impl AsFd,
    ancillary_buf: &mut [u8; rustix::cmsg_space!(ScmRights(1))],
    last_paste: &mut Option<(Mmap, PasteAtom)>,
) -> Result<(), CliError> {
    let mut mime = MimeType::new_const();
    let mut ancillary = RecvAncillaryBuffer::new(ancillary_buf);
    let msg = recvmsg(
        &paste_socket,
        &mut [IoSliceMut::new(unsafe {
            slice::from_raw_parts_mut(mime.as_mut_ptr(), mime.capacity())
        })],
        &mut ancillary,
        RecvFlags::TRUNC,
    )
    .map_io_err(|| "Failed to recv client msg.")?;
    debug_assert!(!msg.flags.contains(RecvFlags::TRUNC));
    debug_assert!(msg.bytes <= mime.capacity());
    unsafe {
        mime.set_len(msg.bytes.min(mime.capacity()));
    }

    let mut mime_atom_req = if mime.is_empty() {
        None
    } else {
        let cookie = conn.intern_atom(false, mime.as_bytes())?;
        conn.flush()?;
        Some(cookie)
    };
    let mut mime_atom = None;

    for msg in ancillary.drain() {
        if let ScmRights(received_fds) = msg {
            for fd in received_fds {
                let data = Mmap::from(fd).map_io_err(|| "Failed to mmap paste file.")?;
                info!("Received paste buffer of length {}.", data.len());
                *last_paste = Some((
                    data,
                    if let Some(a) = mime_atom {
                        a
                    } else if let Some(r) = mime_atom_req.take() {
                        *mime_atom.insert(PasteAtom {
                            atom: r.reply()?.atom,
                            is_text: mime.starts_with("text/"),
                        })
                    } else {
                        PasteAtom {
                            atom: x11rb::NONE,
                            is_text: true,
                        }
                    },
                ));
            }
        }
    }

    debug!("Claiming selection ownership.");
    conn.set_selection_owner(paste_window, clipboard_atom, x11rb::CURRENT_TIME)?
        .check()?;
    Ok(())
}
