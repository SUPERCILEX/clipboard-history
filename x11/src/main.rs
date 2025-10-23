#![feature(if_let_guard)]

use std::{
    borrow::Cow,
    fmt::Display,
    fs::File,
    io::{ErrorKind, Read},
    mem,
    mem::MaybeUninit,
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::FileExt,
    },
    rc::Rc,
    time::Duration,
};

use arrayvec::ArrayVec;
use error_stack::Report;
use log::{debug, error, info, trace, warn};
use ringboard_sdk::{
    api::{AddRequest, MoveToFrontRequest, PasteCommand, connect_to_server},
    config,
    core::{
        Error, IoErr, create_tmp_file,
        dirs::{paste_socket_file, socket_file},
        init_unix_server,
        protocol::{
            AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, Response, RingKind,
        },
        ring::Mmap,
    },
    is_text_mime,
    watcher_utils::{
        best_target::BestMimeTypeFinder,
        deduplication::{CopyData, CopyDeduplication},
        utils::read_paste_command,
    },
};
use rustix::{
    event::epoll,
    fs::{CWD, MemfdFlags, Mode, OFlags, memfd_create},
    io::{Errno, read},
    net::{RecvFlags, SendFlags, SocketAddrUnix, SocketType},
    path::Arg,
    time::{
        Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
        timerfd_settime,
    },
};
use thiserror::Error;
use x11rb::{
    atom_manager,
    connection::{Connection, RequestConnection},
    cookie::Cookie,
    errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError},
    protocol::{
        Event, xfixes,
        xfixes::{SelectionEventMask, select_selection_input},
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, EventMask,
            GetAtomNameReply, GetPropertyType, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, NotifyDetail,
            PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
            SelectionRequestEvent, Window, WindowClass,
        },
        xtest::ConnectionExt as XTestExt,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as WrapperConnExt,
    x11_utils::X11Error,
};

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
    #[error("Serde TOML deserialization failed")]
    Toml(#[from] toml::de::Error),
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
        CliError::Toml(e) => Report::new(e).change_context(wrapper),
    }
}

#[derive(Default, Debug)]
enum State {
    #[default]
    Free,
    FastPathPendingSelection,
    TargetsRequest {
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

const MAX_TRANSFER_SIZE: usize = 1 << 20;

#[derive(Default)]
struct TransferAtomAllocator {
    windows: [Window; MAX_CONCURRENT_TRANSFERS],
    states: [State; MAX_CONCURRENT_TRANSFERS],
    next: u8,
}

impl TransferAtomAllocator {
    fn alloc(&mut self) -> (&mut State, Window, Atom) {
        const _: () = assert!(MAX_CONCURRENT_TRANSFERS.is_power_of_two());

        let next = usize::from(self.next) & (MAX_CONCURRENT_TRANSFERS - 1);

        if !matches!(self.states[next], State::Free) {
            warn!("Too many ongoing transfers, dropping old transfer.");
        }
        let state = &mut self.states[next];
        let transfer_window = self.windows[next];
        let transfer_atom = Self::transfer_atom(next);

        self.next = self.next.wrapping_add(1);
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
        WM_CLASS,
        UTF8_STRING,

        CLIPBOARD,
        PRIMARY,
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

enum PasteFile {
    Small(Mmap),
    Large(Rc<Mmap>),
}

fn load_config() -> Result<config::x11::Latest, CliError> {
    let path = config::x11::file();
    let mut file = match File::open(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(config::x11::Latest::default()),
        r => r.map_io_err(|| format!("Failed to open file: {path:?}"))?,
    };

    let mut config = String::new();
    file.read_to_string(&mut config)
        .map_io_err(|| format!("Failed to read config: {path:?}"))?;
    Ok(toml::from_str::<config::x11::Config>(&config)?.to_latest())
}

fn run() -> Result<(), CliError> {
    info!(
        "Starting Ringboard X11 clipboard listener v{}.",
        env!("CARGO_PKG_VERSION")
    );

    let ref config @ config::x11::Latest {
        auto_paste,
        fast_path_optimizations,
    } = load_config()?;
    info!("Using configuration {config:?}");

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
    let paste_timer = if auto_paste {
        Some(
            timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::empty())
                .map_io_err(|| "Failed to create timer fd.")?,
        )
    } else {
        None
    };
    debug!("Initialized paste server");

    let mut ancillary_buf = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut last_paste = None;
    let mut clear_selection_mask = 0;

    let epoll =
        epoll::create(epoll::CreateFlags::empty()).map_io_err(|| "Failed to create epoll.")?;
    for (i, fd) in [
        Some(conn.stream().as_fd()),
        Some(paste_socket.as_fd()),
        paste_timer.as_ref().map(OwnedFd::as_fd),
    ]
    .iter()
    .flatten()
    .enumerate()
    {
        epoll::add(
            &epoll,
            fd,
            epoll::EventData::new_u64(u64::try_from(i).unwrap()),
            epoll::EventFlags::IN,
        )
        .map_io_err(|| "Failed to register epoll interest.")?;
    }

    let mut allocator = TransferAtomAllocator {
        windows: transfer_windows.into_inner().unwrap(),
        states: [const { State::Free }; MAX_CONCURRENT_TRANSFERS],
        next: 0,
    };
    let mut paste_allocator = Default::default();

    let mut deduplicator = CopyDeduplication::new()?;

    info!("Starting event loop.");
    loop {
        while let Some(event) = conn.poll_for_event()? {
            handle_x11_event(
                event,
                &conn,
                &atoms,
                &mut allocator,
                &server,
                &mut deduplicator,
                fast_path_optimizations,
                paste_window,
                root,
                paste_timer.as_ref(),
                &mut last_paste,
                &mut paste_allocator,
                &mut clear_selection_mask,
            )?;
        }
        conn.flush()?;

        trace!("Waiting for event.");
        let mut epoll_events = [MaybeUninit::uninit(); 3];
        let (epoll_events, _) = match epoll::wait(&epoll, &mut epoll_events, None) {
            Err(Errno::INTR) => continue,
            r => r.map_io_err(|| "Failed to wait for epoll events.")?,
        };

        for &mut epoll::Event { flags: _, data } in epoll_events {
            match data.u64() {
                0 => (),
                1 => handle_paste_event(
                    &conn,
                    &atoms,
                    root,
                    &server,
                    &mut deduplicator,
                    paste_window,
                    &paste_socket,
                    &mut ancillary_buf,
                    &mut last_paste,
                    &mut clear_selection_mask,
                    paste_timer.is_some(),
                )?,
                2 => {
                    read(
                        paste_timer.as_ref().unwrap(),
                        &mut [MaybeUninit::uninit(); 8],
                    )
                    .map_io_err(|| "Failed to clear paste timer.")?;
                    do_paste(&conn, root)?;
                }
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
    fast_path_optimizations: bool,

    paste_window: Window,
    root: Window,
    paste_timer: Option<impl AsFd>,
    last_paste: &mut Option<(PasteFile, PasteAtom)>,
    (paste_alloc_next, paste_allocations, tmp_file_unsupported): &mut (
        u8,
        [(Window, Option<(Atom, Rc<Mmap>, usize)>); MAX_CONCURRENT_TRANSFERS],
        bool,
    ),
    clear_selection_mask: &mut u8,
) -> Result<(), CliError> {
    fn debug_get_atom_name(conn: &RustConnection, atom: Atom) -> Result<impl Display, CliError> {
        if atom == x11rb::NONE {
            Ok(Cow::Borrowed("NONE"))
        } else {
            Ok(String::from_utf8(conn.get_atom_name(atom)?.reply()?.name)
                .map(Cow::Owned)
                .unwrap_or(Cow::Borrowed("INVALID")))
        }
    }

    let &Atoms {
        _NET_WM_NAME: window_name_atom,
        UTF8_STRING: utf8_string_atom,
        CLIPBOARD: clipboard_atom,
        PRIMARY: primary_atom,
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
            debug!(
                "Paste request received for target {target}<{}> on {selection}<{}> selection.",
                debug_get_atom_name(conn, target)?,
                debug_get_atom_name(conn, selection)?
            );
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
                )?;
                Ok(())
            };

            let property = if property == x11rb::NONE {
                debug!("Obsolete client detected.");
                target
            } else {
                property
            };
            if property == x11rb::NONE {
                warn!("Invalid paste request: no property provided to place the data.");
                return reply(x11rb::NONE);
            }

            let reply = |reply_property| {
                if reply_property == x11rb::NONE {
                    conn.delete_property(requestor, property)?;
                }
                reply(reply_property)
            };

            if ![clipboard_atom, primary_atom].contains(&selection) {
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

            if target == targets_atom {
                debug!("Responding to paste request with TARGETS.");
                conn.change_property32(
                    PropMode::REPLACE,
                    requestor,
                    property,
                    AtomEnum::ATOM,
                    &supported_atoms,
                )?;
                return reply(property);
            }

            match paste_file {
                PasteFile::Small(data) => {
                    info!("Responded to paste request with small selection.");
                    conn.change_property8(PropMode::REPLACE, requestor, property, target, data)?;
                }
                PasteFile::Large(data) => {
                    debug!(
                        "Starting paste request INCR transfer for {} bytes.",
                        data.len()
                    );
                    conn.change_window_attributes(
                        requestor,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    )?;
                    conn.change_property32(PropMode::REPLACE, requestor, property, incr_atom, &[
                        u32::try_from(data.len()).unwrap_or(u32::MAX),
                    ])?;

                    if mem::replace(
                        &mut paste_allocations[usize::from(*paste_alloc_next)],
                        (requestor, Some((target, data.clone(), 0))),
                    )
                    .0 != x11rb::NONE
                    {
                        warn!("Too many ongoing paste transfers, dropping oldest paste transfer.");
                    }
                    *paste_alloc_next = (paste_alloc_next.wrapping_add(1))
                        & u8::try_from(paste_allocations.len() - 1).unwrap();
                }
            }
            reply(property)?;
        }
        Event::SelectionNotify(event) if event.requestor == paste_window => {
            error!("Trying to paste into ourselves!");
        }
        Event::SelectionClear(event) => {
            if event.owner != paste_window {
                debug!(
                    "Ignoring selection clear for unknown window: {:?}",
                    event.owner
                );
                return Ok(());
            }
            *clear_selection_mask |= 1
                << if event.selection == clipboard_atom {
                    0
                } else if event.selection == primary_atom {
                    1
                } else {
                    unreachable!()
                };
            trace!("Clear selection mask: {clear_selection_mask:#02b}");

            if *clear_selection_mask == 3 && last_paste.take().is_some() {
                info!("Lost selection ownership.");
            }
        }
        Event::PropertyNotify(event)
            if let Some((requestor, paste_transfer)) = paste_allocations
                .iter_mut()
                .find(|&&mut (requestor, _)| requestor == event.window) =>
        {
            if event.state != Property::DELETE {
                trace!(
                    "Ignoring irrelevant property state change: {:?}.",
                    event.state
                );
                return Ok(());
            }
            let Some((target, data, start)) = paste_transfer else {
                error!("Received property notification after INCR transfer completed.");
                return Ok(());
            };

            let end = start.saturating_add(MAX_TRANSFER_SIZE).min(data.len());
            if *start == end {
                conn.change_window_attributes(
                    event.window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
                )?;
            }
            conn.change_property8(
                PropMode::REPLACE,
                event.window,
                event.atom,
                *target,
                &data[*start..end],
            )?;
            if *start == end {
                info!("Responded to paste request with large selection.");
                *requestor = x11rb::NONE;
                *paste_transfer = None;
            } else {
                debug!(
                    "Continuing INCR transfer with {} bytes remaining.",
                    data.len() - end
                );
                *start = end;
            }
        }
        Event::FocusIn(e) => {
            debug!("Received focus event of type {:?}", e.detail);
            if e.detail == NotifyDetail::NONLINEAR_VIRTUAL {
                conn.change_window_attributes(
                    root,
                    &ChangeWindowAttributesAux::default().event_mask(EventMask::NO_EVENT),
                )?;
                timerfd_settime(
                    paste_timer.unwrap(),
                    TimerfdTimerFlags::empty(),
                    &Itimerspec {
                        it_interval: Timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        },
                        it_value: Timespec {
                            tv_sec: 0,
                            tv_nsec: Duration::from_millis(20).as_nanos().try_into().unwrap(),
                        },
                    },
                )
                .map_io_err(|| "Failed to arm paste timer.")?;
            }
        }

        Event::XfixesSelectionNotify(event) => {
            if event.owner == paste_window {
                debug!("Ignoring selection notification from ourselves.");
                return Ok(());
            }

            info!("Selection notification received.");
            let (state, transfer_window, transfer_atom) = allocator.alloc();
            *state = if fast_path_optimizations {
                State::FastPathPendingSelection
            } else {
                State::TargetsRequest {
                    allow_plain_text: true,
                }
            };
            trace!("Initialized transfer state for atom {transfer_atom}: {state:?}");

            conn.convert_selection(
                transfer_window,
                event.selection,
                if fast_path_optimizations {
                    utf8_string_atom
                } else {
                    targets_atom
                },
                transfer_atom,
                x11rb::CURRENT_TIME,
            )?;
        }
        Event::SelectionNotify(event) => {
            let Some((state, transfer_atom)) = allocator.get(event.requestor) else {
                warn!(
                    "Ignoring selection notification to unknown requester {}.",
                    event.requestor
                );
                return Ok(());
            };
            trace!(
                "Stage 2 selection notification received for atom {}<{}>: {state:?}.",
                event.property,
                debug_get_atom_name(conn, event.property)?,
            );

            let property = if event.property == x11rb::NONE {
                None
            } else {
                let property = conn.get_property(
                    true,
                    event.requestor,
                    event.property,
                    GetPropertyType::ANY,
                    0,
                    u32::MAX,
                )?;
                conn.flush()?;
                Some(property)
            };

            match mem::take(state) {
                State::TargetsRequest { allow_plain_text } => {
                    let Some(property) = property else {
                        warn!("Targets response cancelled.");
                        return Ok(());
                    };

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
                        finder.block_plain_text();
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
                        event.requestor,
                        event.selection,
                        target,
                        transfer_atom,
                        x11rb::CURRENT_TIME,
                    )?;
                }
                s @ (State::FastPathPendingSelection | State::PendingSelection { .. }) => {
                    let Some(property) = property else {
                        match s {
                            State::FastPathPendingSelection => {
                                debug!(
                                    "UTF8_STRING target fast path failed. Retrying with target \
                                     query."
                                );
                                *state = State::TargetsRequest {
                                    allow_plain_text: true,
                                };
                                conn.convert_selection(
                                    event.requestor,
                                    event.selection,
                                    targets_atom,
                                    transfer_atom,
                                    x11rb::CURRENT_TIME,
                                )?;
                            }
                            State::PendingSelection { .. } => {
                                warn!("Selection transfer cancelled.");
                            }
                            _ => unreachable!(),
                        }
                        return Ok(());
                    };

                    let (mime_atom, mime_type, fast_path) = match s {
                        State::FastPathPendingSelection => {
                            (utf8_string_atom, MimeType::new_const(), true)
                        }
                        State::PendingSelection {
                            mime_atom,
                            mime_type,
                        } => (mime_atom, mime_type, false),
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
                            if fast_path {
                                debug!(
                                    "UTF8_STRING target fast path empty or blank. Retrying with \
                                     target query."
                                );
                                *state = State::TargetsRequest {
                                    allow_plain_text: false,
                                };
                                conn.convert_selection(
                                    event.requestor,
                                    event.selection,
                                    targets_atom,
                                    transfer_atom,
                                    x11rb::CURRENT_TIME,
                                )?;
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
                s @ (State::PendingIncr { .. } | State::Free) => {
                    error!(
                        "Received selection notification for {} atom {}<{}>.",
                        if matches!(s, State::Free) {
                            "free"
                        } else {
                            "incr"
                        },
                        event.property,
                        debug_get_atom_name(conn, event.property)?,
                    );
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
                    "Ignoring irrelevant property state change: {:?}.",
                    event.state
                );
                return Ok(());
            }
            let Some((state, _)) = allocator.get(event.window) else {
                warn!(
                    "Ignoring property notify to unknown requester {}.",
                    event.window
                );
                return Ok(());
            };

            trace!(
                "Processing property notification for atom {}<{}>: {state:?}",
                event.atom,
                debug_get_atom_name(conn, event.atom)?,
            );
            match state {
                State::PendingIncr { .. } => {
                    let State::PendingIncr {
                        mime_atom,
                        mime_type,
                        file,
                        written,
                    } = mem::take(state)
                    else {
                        unreachable!()
                    };
                    let property = conn.get_property(
                        true,
                        event.window,
                        event.atom,
                        GetPropertyType::ANY,
                        0,
                        u32::MAX,
                    )?;
                    conn.flush()?;

                    let file = if let Some(file) = file {
                        file
                    } else {
                        File::from(
                            create_tmp_file(
                                tmp_file_unsupported,
                                CWD,
                                c".",
                                c".ringboard-x11-scratchpad",
                                OFlags::RDWR,
                                Mode::empty(),
                            )
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
                State::FastPathPendingSelection
                | State::TargetsRequest { .. }
                | State::PendingSelection { .. } => {
                    trace!("Ignoring property to be processed in selection notification.");
                }
                State::Free => {
                    error!(
                        "Received property notification for free atom {}<{}>.",
                        event.atom,
                        debug_get_atom_name(conn, event.atom)?,
                    );
                }
            }
        }
        Event::Error(e) => return Err(e.into()),
        event => {
            trace!("Ignoring irrelevant X11 event: {event:?}");
        }
    }
    Ok(())
}

fn handle_paste_event(
    conn: &RustConnection,
    atoms: &Atoms,
    root: Window,
    server: impl AsFd,
    deduplicator: &mut CopyDeduplication,
    paste_window: Window,
    paste_socket: impl AsFd,
    ancillary_buf: &mut [MaybeUninit<u8>; rustix::cmsg_space!(ScmRights(1))],
    last_paste: &mut Option<(PasteFile, PasteAtom)>,
    clear_selection_mask: &mut u8,
    auto_paste: bool,
) -> Result<(), CliError> {
    struct MoveToFrontGuard<'a, 'b, Server: AsFd>(
        Server,
        &'a mut Option<(PasteFile, PasteAtom)>,
        &'b mut CopyDeduplication,
    );

    impl<Server: AsFd> Drop for MoveToFrontGuard<'_, '_, Server> {
        fn drop(&mut self) {
            let Ok(MoveToFrontResponse::Success { id }) =
                unsafe { MoveToFrontRequest::recv(&self.0, RecvFlags::empty()) }.map(
                    |Response {
                         sequence_number: _,
                         value,
                     }| value,
                )
            else {
                return;
            };
            let Some((file, _)) = self.1 else {
                return;
            };

            let data = match file {
                PasteFile::Small(mmap) => mmap,
                PasteFile::Large(mmap) => &**mmap,
            };
            let data_hash =
                CopyDeduplication::hash(CopyData::Slice(data), u64::try_from(data.len()).unwrap());
            debug!("Pasted entry promoted to front.");
            self.2.remember(data_hash, id);
        }
    }

    let (
        cmd @ PasteCommand {
            trigger_paste,
            id,
            mime,
            ..
        },
        fd,
    ) = read_paste_command(paste_socket, ancillary_buf)?;
    debug!("Received paste command: {cmd:?}");

    MoveToFrontRequest::send(&server, id, None, SendFlags::empty())?;
    let move_to_front_guard = MoveToFrontGuard(server, last_paste, deduplicator);

    let mut mime_atom_req = if mime.is_empty() {
        None
    } else {
        let cookie = conn.intern_atom(false, mime.as_bytes())?;
        conn.flush()?;
        Some(cookie)
    };
    let mut mime_atom = None;

    if let Some(fd) = fd {
        let data = Mmap::from(fd).map_io_err(|| "Failed to mmap paste file.")?;
        info!("Received paste buffer of length {}.", data.len());
        *move_to_front_guard.1 = Some((
            if data.len() > MAX_TRANSFER_SIZE {
                PasteFile::Large(Rc::new(data))
            } else {
                PasteFile::Small(data)
            },
            if let Some(a) = mime_atom {
                a
            } else if let Some(r) = mime_atom_req.take() {
                *mime_atom.insert(PasteAtom {
                    atom: r.reply()?.atom,
                    is_text: is_text_mime(&mime),
                })
            } else {
                PasteAtom {
                    atom: x11rb::NONE,
                    is_text: true,
                }
            },
        ));
    }

    let Atoms {
        CLIPBOARD: clipboard_atom,
        PRIMARY: primary_atom,
        WM_CLASS: window_class_atom,
        ..
    } = *atoms;

    debug!("Claiming selection ownership.");
    conn.set_selection_owner(paste_window, clipboard_atom, x11rb::CURRENT_TIME)?;
    conn.set_selection_owner(paste_window, primary_atom, x11rb::CURRENT_TIME)?;
    *clear_selection_mask = 0;

    if auto_paste && trigger_paste {
        trace!("Preparing to send paste command.");
        let focused_window = conn.get_input_focus()?.reply()?.focus;
        let should_defer = || -> Result<bool, CliError> {
            let class = conn
                .get_property(
                    false,
                    focused_window,
                    window_class_atom,
                    GetPropertyType::ANY,
                    0,
                    u32::MAX,
                )?
                .reply()?;
            let Some(name) = class.value.split(|&b| b == 0).nth(1) else {
                return Ok(false);
            };
            if name != b"ringboard-egui" {
                return Ok(false);
            }

            conn.change_window_attributes(
                root,
                &ChangeWindowAttributesAux::default().event_mask(EventMask::FOCUS_CHANGE),
            )?;

            Ok(true)
        };
        if should_defer().ok() == Some(true) {
            debug!("Waiting for focus event to send paste command.");
        } else {
            do_paste(conn, root)?;
        }
    }

    Ok(())
}

fn do_paste(conn: &RustConnection, root: Window) -> Result<(), CliError> {
    let key = |type_, code| conn.xtest_fake_input(type_, code, x11rb::CURRENT_TIME, root, 1, 1, 0);

    // Shift + Insert
    key(KEY_PRESS_EVENT, 50)?;
    key(KEY_PRESS_EVENT, 118)?;
    key(KEY_RELEASE_EVENT, 118)?;
    key(KEY_RELEASE_EVENT, 50)?;
    conn.flush()?;
    info!("Sent paste command.");

    Ok(())
}
