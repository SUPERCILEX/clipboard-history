#![feature(let_chains)]

use std::{
    fs::File,
    mem,
    os::{fd::AsFd, unix::fs::FileExt},
};

use arrayvec::ArrayVec;
use error_stack::Report;
use log::{debug, error, info, trace, warn};
use ringboard_sdk::{
    api::{connect_to_server, AddRequest, MoveToFrontRequest},
    core::{
        dirs::socket_file,
        protocol::{AddResponse, MimeType, MoveToFrontResponse, RingKind},
        Error, IoErr,
    },
};
use rustix::{
    fs::{memfd_create, openat, MemfdFlags, Mode, OFlags, CWD},
    net::SocketAddrUnix,
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
            GetPropertyType, PropMode, Property, Window, WindowClass,
        },
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as WrapperConnExt,
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
    #[error("Failed to connect to X11 server.")]
    X11Connect(#[from] ConnectError),
    #[error("X11 request failed.")]
    X11Connection(#[from] ConnectionError),
    #[error("X11 reply failed.")]
    X11Reply(#[from] ReplyError),
    #[error("Failed to create X11 ID.")]
    X11Id(#[from] ReplyOrIdError),
    #[error("Unsupported X11: XFixes extension not available.")]
    X11NoXfixes,
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
        CliError::X11Reply(e) => Report::new(e).change_context(wrapper),
        CliError::X11Id(e) => Report::new(e).change_context(wrapper),
        CliError::X11NoXfixes => Report::new(wrapper),
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
    },
    PendingIncr {
        mime_atom: Atom,
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
    }
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

    let create_window = |title: &[u8], aux| -> Result<_, CliError> {
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
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &aux,
        )?
        .check()?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            window_name_atom,
            utf8_string_atom,
            title,
        )?
        .check()?;
        Ok(window)
    };
    let mut transfer_windows = ArrayVec::<_, MAX_CONCURRENT_TRANSFERS>::new_const();
    for i in 0..MAX_CONCURRENT_TRANSFERS {
        transfer_windows.push(create_window(
            format!("Ringboard data transfer {}", i + 1).as_bytes(),
            CreateWindowAux::default().event_mask(EventMask::PROPERTY_CHANGE),
        )?);
    }
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
    )?
    .check()?;
    debug!("Selection owner listener registered.");

    let mut allocator = TransferAtomAllocator {
        windows: transfer_windows.into_inner().unwrap(),
        states: [const { State::Free }; MAX_CONCURRENT_TRANSFERS],
        next: 0,
    };

    let mut deduplicator = CopyDeduplication::new()?;

    info!("Starting event loop.");
    loop {
        trace!("Waiting for event.");
        handle_x11_event(
            conn.wait_for_event()?,
            &conn,
            &atoms,
            &mut allocator,
            &server,
            &mut deduplicator,
        )?;
    }
}

fn handle_x11_event(
    event: Event,
    conn: &RustConnection,
    atoms: &Atoms,
    allocator: &mut TransferAtomAllocator,
    server: impl AsFd,
    deduplicator: &mut CopyDeduplication,
) -> Result<(), CliError> {
    let &Atoms {
        _NET_WM_NAME: window_name_atom,
        UTF8_STRING: utf8_string_atom,
        TARGETS: targets_atom,
        INCR: incr_atom,
        ..
    } = atoms;
    let mut pending_atom_cookies = ArrayVec::<(Cookie<_, GetAtomNameReply>, _), 8>::new_const();

    match event {
        Event::XfixesSelectionNotify(event) => {
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
            if event.state != Property::NEW_VALUE {
                trace!(
                    "Ignoring uninteresting property state change: {:?}.",
                    event.state
                );
                return Ok(());
            }
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
                    let property = conn
                        .get_property(
                            true,
                            event.window,
                            event.atom,
                            GetPropertyType::ANY,
                            0,
                            u32::MAX,
                        )?
                        .reply()?;
                    if property.type_ == incr_atom {
                        warn!("Ignoring abusive TARGETS property.");
                        return Ok(());
                    }

                    let Some(mut value) = property.value32() else {
                        error!("Invalid TARGETS property value format.");
                        return Ok(());
                    };

                    let mut finder = BestMimeTypeFinder::default();
                    loop {
                        let atom = value.next();
                        if pending_atom_cookies.is_full() || atom.is_none() {
                            for (cookie, atom) in pending_atom_cookies.drain(..) {
                                let reply = cookie.reply()?;
                                let name = reply.name.to_string_lossy();
                                debug!("Target {name:?} available on atom {atom}.");

                                if name.len() > MimeType::new_const().capacity() {
                                    warn!("Target {name:?} name too long, ignoring.");
                                    continue;
                                }

                                finder.add_mime(&name, atom);
                            }
                        }
                        let Some(atom) = atom else {
                            break;
                        };
                        pending_atom_cookies.push((conn.get_atom_name(atom)?, atom));
                    }

                    if !allow_plain_text {
                        debug!(
                            "Blocking plain text as it returned a blank or empty result on the \
                             fast path."
                        );
                        finder.kill_text();
                    }
                    let target = finder.best();
                    let Some(target) = target else {
                        warn!("No usable targets returned, dropping selection.");
                        return Ok(());
                    };
                    if cfg!(debug_assertions) {
                        info!(
                            "Choosing target {:?} on atom {target}.",
                            conn.get_atom_name(target)?.reply()?.name.to_string_lossy()
                        );
                    } else {
                        info!("Choosing atom {:?}.", target);
                    }

                    *state = State::PendingSelection { mime_atom: target };
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
                    let (mime_atom, fast_path) = match s {
                        State::FastPathPendingSelection { selection } => {
                            (utf8_string_atom, Some(selection))
                        }
                        State::PendingSelection { mime_atom } => (mime_atom, None),
                        _ => unreachable!(),
                    };

                    let property = conn
                        .get_property(
                            true,
                            event.window,
                            event.atom,
                            GetPropertyType::ANY,
                            0,
                            u32::MAX,
                        )?
                        .reply()?;
                    if property.type_ == incr_atom {
                        debug!("Waiting for INCR transfer.");
                        *state = State::PendingIncr {
                            mime_atom,
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

                        let mime_type = conn.get_atom_name(mime_atom)?;
                        let file = File::from(
                            memfd_create(c"ringboard_x11_selection", MemfdFlags::empty())
                                .map_io_err(|| "Failed to create selection transfer temp file.")?,
                        );
                        file.write_all_at(&property.value, 0)
                            .map_io_err(|| "Failed to write data to temp file.")?;
                        let mime_type =
                            MimeType::from(&mime_type.reply()?.name.to_string_lossy()).unwrap();

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
                    file,
                    written,
                } => {
                    let property = conn.get_property(
                        true,
                        event.window,
                        event.atom,
                        GetPropertyType::ANY,
                        0,
                        u32::MAX,
                    )?;
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

                        let mime_type = MimeType::from(
                            &conn
                                .get_atom_name(mime_atom)?
                                .reply()?
                                .name
                                .to_string_lossy(),
                        )
                        .unwrap();

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
                            file: Some(file),
                            written: written + u64::try_from(property.value.len()).unwrap(),
                        }
                    }
                }
                State::Free => {
                    if event.atom != window_name_atom {
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
        }
        Event::Error(e) => {
            error!("Unexpected X11 event error: {e:?}");
        }
        event => {
            debug!("Ignoring unknown X11 event: {event:?}");
        }
    }
    Ok(())
}
