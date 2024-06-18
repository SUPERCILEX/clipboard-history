use std::{fs::File, mem, ops::Deref, os::unix::fs::FileExt};

use arrayvec::ArrayVec;
use error_stack::Report;
use log::{debug, error, info, trace, warn};
use ringboard_sdk::core::{
    dirs::socket_file,
    protocol::{AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, RingKind},
    Error, IoErr,
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
            GetPropertyType, PropMode, Property, PropertyNotifyEvent, SelectionNotifyEvent,
            WindowClass,
        },
        Event,
    },
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
    #[error("Unsupported X11: xfixes extension not available.")]
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
        CliError::Core(e) | CliError::Sdk(ringboard_sdk::ClientError::Core(e)) => match e {
            Error::Io { error, context } => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            Error::NotARingboard { file: _ } => Report::new(wrapper),
            Error::InvalidPidError { error, context } => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            Error::IdNotFound(IdNotFoundError::Ring(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown ring: {id}"))
            }
            Error::IdNotFound(IdNotFoundError::Entry(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown entry: {id}"))
            }
        },
        CliError::Sdk(ringboard_sdk::ClientError::InvalidResponse { context }) => {
            Report::new(wrapper).attach_printable(context)
        }
        CliError::Sdk(ringboard_sdk::ClientError::VersionMismatch { actual: _ }) => {
            Report::new(wrapper)
        }
        CliError::X11Connect(e) => Report::new(e).change_context(wrapper),
        CliError::X11Connection(e) => Report::new(e).change_context(wrapper),
        CliError::X11Reply(e) => Report::new(e).change_context(wrapper),
        CliError::X11Id(e) => Report::new(e).change_context(wrapper),
        CliError::X11NoXfixes => Report::new(wrapper),
    }
}

#[derive(Default)]
enum TransferAtom {
    #[default]
    Free,
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
struct TransferAtomAllocation(usize);

impl TransferAtomAllocation {
    #[must_use]
    fn advance(&mut self) -> Atom {
        const _: () = assert!(MAX_CONCURRENT_TRANSFERS.is_power_of_two());

        let old = self.0;
        self.0 += 1;
        self.0 &= MAX_CONCURRENT_TRANSFERS - 1;
        u32::try_from(old).unwrap() + u32::from(BASE_TRANSFER_ATOM)
    }

    fn from_atom(atom: Atom) -> Option<usize> {
        (atom >= BASE_TRANSFER_ATOM.into()
            && atom
                < u32::try_from(MAX_CONCURRENT_TRANSFERS).unwrap() + u32::from(BASE_TRANSFER_ATOM))
        .then(|| usize::try_from(atom - u32::from(BASE_TRANSFER_ATOM)).unwrap())
    }
}

impl Deref for TransferAtomAllocation {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run() -> Result<(), CliError> {
    info!(
        "Starting Ringboard X11 clipboard listener v{}.",
        env!("CARGO_PKG_VERSION")
    );

    let server_addr = {
        let socket_file = socket_file();
        SocketAddrUnix::new(&socket_file)
            .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?
    };
    let server = ringboard_sdk::connect_to_server(&server_addr)?;
    debug!("Ringboard connection established.");

    let (conn, root) = {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;

        (conn, root)
    };
    debug!("X11 connection established.");

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
    let Atoms {
        _NET_WM_NAME: window_name_atom,
        UTF8_STRING: utf8_string_atom,
        CLIPBOARD: clipboard_atom,
        TARGETS: targets_atom,
        INCR: incr_atom,
    } = Atoms::new(&conn)?.reply()?;
    debug!("Atom internment complete.");

    let create_window = |title, aux| -> Result<_, CliError> {
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
    let targets_window = create_window(
        b"Ringboard selection notification",
        CreateWindowAux::default(),
    )?;
    let transfer_window = create_window(
        b"Ringboard data transfer",
        CreateWindowAux::default().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    debug!("Created utility windows.");

    conn.extension_information(xfixes::X11_EXTENSION_NAME)?
        .ok_or(CliError::X11NoXfixes)?;
    xfixes::query_version(&conn, 5, 0)?.reply()?;
    debug!("Xfixes extension availability confirmed.");

    select_selection_input(
        &conn,
        root,
        clipboard_atom,
        SelectionEventMask::SET_SELECTION_OWNER,
    )?
    .check()?;
    debug!("Selection owner listener registered.");

    let mut pending_atom_cookies = ArrayVec::<(Cookie<_, GetAtomNameReply>, _), 8>::new_const();
    let mut transfer_atoms =
        ArrayVec::from([const { TransferAtom::Free }; MAX_CONCURRENT_TRANSFERS]);
    let mut transfer_atoms_allocator = TransferAtomAllocation::default();

    let mut deduplicator = CopyDeduplication::new()?;

    info!("Starting event loop.");
    loop {
        trace!("Waiting for event.");
        match conn.wait_for_event()? {
            Event::XfixesSelectionNotify(event) => {
                info!("Selection notification received.");
                conn.convert_selection(
                    targets_window,
                    event.selection,
                    targets_atom,
                    Atom::from(BASE_TRANSFER_ATOM)
                        + u32::try_from(MAX_CONCURRENT_TRANSFERS).unwrap(),
                    x11rb::CURRENT_TIME,
                )?
                .check()?;
            }
            Event::SelectionNotify(event) if event.requestor == targets_window => {
                if event.property == x11rb::NONE {
                    warn!("Targets response cancelled.");
                    continue;
                }

                let property = conn
                    .get_property(
                        true,
                        event.requestor,
                        event.property,
                        GetPropertyType::ANY,
                        0,
                        u32::MAX,
                    )?
                    .reply()?;
                if property.type_ == incr_atom {
                    warn!("Ignoring abusive TARGETS property.");
                    continue;
                }

                let Some(mut value) = property.value32() else {
                    error!("Invalid TARGETS property value format.");
                    continue;
                };

                let mut finder = BestMimeTypeFinder::default();
                loop {
                    let atom = value.next();
                    if pending_atom_cookies.is_full() || atom.is_none() {
                        for (cookie, atom) in pending_atom_cookies.drain(..) {
                            let reply = cookie.reply()?;
                            let name = reply.name.to_string_lossy();
                            trace!("Target {name:?} available on atom {atom}.");

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

                let target = finder.best();
                if target.is_none() {
                    warn!("No usable targets returned, asking for plain text anyways.");
                }
                let target = target.unwrap_or(utf8_string_atom);
                if cfg!(debug_assertions) {
                    info!(
                        "Choosing target {:?} on atom {target}.",
                        conn.get_atom_name(target)?.reply()?.name.to_string_lossy()
                    );
                } else {
                    info!("Choosing atom {:?}.", target);
                }

                if !matches!(
                    transfer_atoms[*transfer_atoms_allocator],
                    TransferAtom::Free
                ) {
                    warn!("Too many ongoing transfers, dropping old transfer.");
                }
                transfer_atoms[*transfer_atoms_allocator] =
                    TransferAtom::PendingSelection { mime_atom: target };
                let transfer_atom = transfer_atoms_allocator.advance();

                conn.convert_selection(
                    transfer_window,
                    event.selection,
                    target,
                    transfer_atom,
                    x11rb::CURRENT_TIME,
                )?
                .check()?;
            }
            Event::SelectionNotify(event) if event.requestor == transfer_window => {
                if event.property == x11rb::NONE {
                    warn!("Selection transfer cancelled.");
                    continue;
                }

                debug!("Received selection notification.");
            }
            Event::PropertyNotify(event) if event.window == transfer_window => {
                if event.state != Property::NEW_VALUE {
                    trace!(
                        "Ignoring unuseful property state change: {:?}.",
                        event.state
                    );
                    continue;
                }
                let Some(atom_allocation) = TransferAtomAllocation::from_atom(event.atom) else {
                    debug!(
                        "Ignoring spurious property change event on atom {}.",
                        event.atom
                    );
                    continue;
                };
                match mem::take(&mut transfer_atoms[atom_allocation]) {
                    TransferAtom::PendingSelection { mime_atom } => {
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
                            transfer_atoms[atom_allocation] = TransferAtom::PendingIncr {
                                mime_atom,
                                file: None,
                                written: 0,
                            };
                        } else {
                            if property.value.is_empty() {
                                warn!("Dropping empty selection.");
                                continue;
                            }
                            if property.value.iter().all(u8::is_ascii_whitespace) {
                                warn!("Dropping blank selection.");
                                continue;
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
                                    ringboard_sdk::move_to_front(
                                        &server,
                                        &server_addr,
                                        existing,
                                        None,
                                    )?
                                {
                                    deduplicator.remember(data_hash, id);
                                    continue;
                                }
                            }

                            let mime_type = conn.get_atom_name(mime_atom)?;
                            let file = File::from(
                                memfd_create("ringboard_x11_selection", MemfdFlags::empty())
                                    .map_io_err(|| {
                                        "Failed to create selection transfer temp file."
                                    })?,
                            );
                            file.write_all_at(&property.value, 0)
                                .map_io_err(|| "Failed to write data to temp file.")?;
                            let mime_type =
                                MimeType::from(&mime_type.reply()?.name.to_string_lossy()).unwrap();

                            let AddResponse::Success { id } = ringboard_sdk::add(
                                &server,
                                &server_addr,
                                RingKind::Main,
                                mime_type,
                                file,
                            )?;
                            deduplicator.remember(data_hash, id);
                            info!("Small selection transfer complete.");
                        }
                    }
                    TransferAtom::PendingIncr {
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
                                    .map_io_err(|| {
                                        "Failed to create selection transfer temp file."
                                    })?,
                            )
                        };

                        let property = property.reply()?;
                        if property.value.is_empty() {
                            if written == 0 {
                                warn!("Dropping empty INCR selection.");
                                continue;
                            }

                            let data_hash = CopyDeduplication::hash(CopyData::File(&file), written);
                            if let Some(existing) =
                                deduplicator.check(data_hash, CopyData::File(&file))
                            {
                                info!("Promoting duplicate large selection to front.");
                                if let MoveToFrontResponse::Success { id } =
                                    ringboard_sdk::move_to_front(
                                        &server,
                                        &server_addr,
                                        existing,
                                        None,
                                    )?
                                {
                                    deduplicator.remember(data_hash, id);
                                    continue;
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

                            let AddResponse::Success { id } = ringboard_sdk::add(
                                &server,
                                &server_addr,
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
                            transfer_atoms[atom_allocation] = TransferAtom::PendingIncr {
                                mime_atom,
                                file: Some(file),
                                written: written + u64::try_from(property.value.len()).unwrap(),
                            }
                        }
                    }
                    TransferAtom::Free => {
                        error!(
                            "Received property notification for free atom {}.",
                            event.atom
                        );
                    }
                }
            }
            Event::SelectionNotify(SelectionNotifyEvent {
                requestor: window, ..
            })
            | Event::PropertyNotify(PropertyNotifyEvent { window, .. }) => {
                warn!(
                    "Ignoring selection response to unknown requester {}.",
                    window
                );
            }
            Event::Error(e) => {
                error!("Unexpected X11 event error: {e:?}");
            }
            event => {
                debug!("Ignoring unknown X11 event: {event:?}");
            }
        };
    }
}
