use std::{
    collections::HashMap,
    convert::identity,
    fmt::{Debug, Formatter},
    fs::File,
    hash::BuildHasherDefault,
    io,
    io::{ErrorKind, ErrorKind::WouldBlock, Read},
    mem,
    mem::{ManuallyDrop, MaybeUninit},
    ops::Deref,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    rc::Rc,
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
        init_unix_server, is_plaintext_mime,
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
use rustc_hash::FxHasher;
use rustix::{
    event::epoll,
    fs::{CWD, MemfdFlags, Mode, OFlags, memfd_create},
    io::Errno,
    net::{RecvFlags, SendFlags, SocketAddrUnix, SocketType},
    pipe::{SpliceFlags, pipe, splice},
};
use thiserror::Error;
use wayland_client::{
    ConnectError, Connection, Dispatch, DispatchError, Proxy, QueueHandle,
    backend::WaylandError,
    event_created_child,
    protocol::{
        wl_keyboard::{KeyState, WlKeyboard},
        wl_registry,
        wl_registry::WlRegistry,
        wl_seat,
        wl_seat::WlSeat,
    },
};
use wayland_protocols::ext::{
    data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
        ext_data_control_source_v1::{self, ExtDataControlSourceV1},
    },
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ext_foreign_toplevel_list_v1,
        ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
    },
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] Error),
    #[error("{0}")]
    Sdk(#[from] ringboard_sdk::ClientError),
    #[error("Wayland connection: {0}")]
    WaylandConnection(#[from] ConnectError),
    #[error("Wayland dispatch: {0}")]
    WaylandDispatch(#[from] DispatchError),
    #[error("{message}: {interface}")]
    BadWaylandGlobal {
        message: &'static str,
        interface: &'static str,
    },
    #[error("Serde TOML deserialization failed")]
    Toml(#[from] toml::de::Error),
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

fn main() -> Result<(), Report<Wrapper>> {
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
        CliError::WaylandConnection(e) => Report::new(e).change_context(wrapper),
        CliError::WaylandDispatch(e) => Report::new(e).change_context(wrapper),
        CliError::BadWaylandGlobal {
            message: _,
            interface: _,
        } => Report::new(wrapper),
        CliError::Toml(e) => Report::new(e).change_context(wrapper),
    }
}

fn load_config() -> Result<config::wayland::Config, CliError> {
    let path = config::wayland::file();
    let mut file = match File::open(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(config::wayland::Config::default()),
        r => r.map_io_err(|| format!("Failed to open file: {path:?}"))?,
    };

    let mut config = String::new();
    file.read_to_string(&mut config)
        .map_io_err(|| format!("Failed to read config: {path:?}"))?;
    Ok(toml::from_str::<config::wayland::Stable>(&config)?.into())
}

fn run() -> Result<(), CliError> {
    info!(
        "Starting Ringboard Wayland clipboard listener v{}.",
        env!("CARGO_PKG_VERSION")
    );

    let ref config @ config::wayland::Config { auto_paste } = load_config()?;
    info!("Using configuration {config:?}");

    let server = {
        let socket_file = socket_file();
        let addr = SocketAddrUnix::new(&socket_file)
            .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?;
        connect_to_server(&addr)?
    };
    debug!("Ringboard connection established.");

    let conn = Connection::connect_to_env()?;
    debug!("Wayland connection established.");

    let paste_socket = init_unix_server(paste_socket_file(), SocketType::DGRAM)?;
    debug!("Initialized paste server");

    let mut ancillary_buf = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];

    let epoll =
        epoll::create(epoll::CreateFlags::empty()).map_io_err(|| "Failed to create epoll.")?;
    for (i, fd) in [conn.as_fd(), paste_socket.as_fd()].iter().enumerate() {
        epoll::add(
            &epoll,
            fd,
            epoll::EventData::new_u64(
                u64::try_from(i + IN_TRANSFER_BUFFERS + OUT_TRANSFER_BUFFERS).unwrap(),
            ),
            epoll::EventFlags::IN,
        )
        .map_io_err(|| "Failed to register epoll interest.")?;
    }
    let mut app = App {
        inner: AppDefault::default(),
        epoll,
    };

    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    conn.display().get_registry(&qh, ());
    drop(conn);
    event_queue.roundtrip(&mut app)?;

    if let Some(e) = app.inner.error {
        return Err(e);
    }
    if app.inner.manager.is_none() {
        return Err(CliError::BadWaylandGlobal {
            message: "compositor does not implement necessary interface",
            interface: "ext_data_control_manager_v1",
        });
    }
    if app.inner.virtual_keyboard_manager.is_none() {
        warn!("Virtual keyboard protocol not available: auto-paste will not work.");
    }
    if app.inner.foreign_toplevels.is_none() {
        warn!("Foreign toplevel protocol not available: auto-paste will not work.");
    }
    debug!("Wayland globals initialized.");

    let mut deduplicator = CopyDeduplication::new()?;

    info!("Starting event loop.");
    loop {
        if let Some(e) = app.inner.error {
            return Err(e);
        }
        event_queue.flush().map_err(DispatchError::from)?;

        trace!("Waiting for event.");
        let mut epoll_events = [MaybeUninit::uninit(); 4];
        let (epoll_events, _) = match epoll::wait(&app.epoll, &mut epoll_events, None) {
            Err(Errno::INTR) => continue,
            r => r.map_io_err(|| "Failed to wait for epoll events.")?,
        };
        for &mut epoll::Event { flags: _, data } in epoll_events {
            const OUT_START_IDX: u64 = IN_TRANSFER_BUFFERS as u64;
            const WAYLAND_IDX: u64 = OUT_START_IDX + OUT_TRANSFER_BUFFERS as u64;
            const PASTE_SERVER_IDX: u64 = WAYLAND_IDX + 1;
            match data.u64() {
                idx @ ..OUT_START_IDX => app.inner.pending_offers.continue_transfer(
                    &mut app.inner.tmp_file_unsupported,
                    &server,
                    &app.epoll,
                    &mut deduplicator,
                    usize::try_from(idx).unwrap(),
                )?,
                idx @ OUT_START_IDX..WAYLAND_IDX => app
                    .inner
                    .outgoing_transfers
                    .continue_transfer(usize::try_from(idx).unwrap() - OUT_TRANSFER_BUFFERS)?,
                WAYLAND_IDX => {
                    trace!("Wayland event received.");
                    let count = match event_queue.prepare_read().unwrap().read() {
                        Err(WaylandError::Io(e)) if e.kind() == WouldBlock => continue,
                        r => r.map_err(DispatchError::from)?,
                    };
                    trace!("Prepared {count} events.");
                    event_queue.dispatch_pending(&mut app)?;
                    trace!("Dispatched {count} events.");
                }
                PASTE_SERVER_IDX => handle_paste_event(
                    &paste_socket,
                    &mut ancillary_buf,
                    &qh,
                    app.inner.manager.as_ref(),
                    &app.inner.seats,
                    auto_paste,
                    &mut app.inner.pending_paste,
                    &mut app.inner.sources,
                    &server,
                    &mut deduplicator,
                )?,
                _ => unreachable!(),
            }
        }
    }
}

trait Destroyable {
    fn destroy(&self);
}

impl Destroyable for WlSeat {
    fn destroy(&self) {
        self.release();
    }
}

impl Destroyable for ExtDataControlManagerV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ExtDataControlDeviceV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ExtDataControlOfferV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ExtDataControlSourceV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for WlKeyboard {
    fn destroy(&self) {
        self.release();
    }
}

impl Destroyable for ZwpVirtualKeyboardV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ExtForeignToplevelListV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ExtForeignToplevelHandleV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

struct AutoDestroy<T: Destroyable>(T);

impl<T: Destroyable + Debug> Debug for AutoDestroy<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: Destroyable> Deref for AutoDestroy<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Destroyable> Drop for AutoDestroy<T> {
    fn drop(&mut self) {
        self.destroy();
    }
}

type SeatStore = (
    AutoDestroy<WlSeat>,
    AutoDestroy<ExtDataControlDeviceV1>,
    AutoDestroy<WlKeyboard>,
    Option<AutoDestroy<ZwpVirtualKeyboardV1>>,
);

#[derive(Default, Debug)]
struct Seats {
    active: u32,
    first: Option<(u32, SeatStore)>,
    others: HashMap<u32, SeatStore, BuildHasherDefault<FxHasher>>,
}

impl Seats {
    fn add(
        &mut self,
        seat: u32,
        seat_obj: WlSeat,
        device: ExtDataControlDeviceV1,
        keyboard: WlKeyboard,
    ) {
        let Self {
            active,
            first,
            others,
        } = self;

        let value = (
            AutoDestroy(seat_obj),
            AutoDestroy(device),
            AutoDestroy(keyboard),
            None,
        );
        if first.is_none() {
            *first = Some((seat, value));
            *active = seat;
        } else if others.insert(seat, value).is_some() {
            error!("Duplicate seat: {seat}");
        }
    }

    fn get(&self, seat: u32) -> Option<&SeatStore> {
        let Self {
            active: _,
            first,
            others,
        } = self;

        if let &Some((existing, ref value)) = first
            && seat == existing
        {
            Some(value)
        } else {
            others.get(&seat)
        }
    }

    fn get_mut(&mut self, seat: u32) -> Option<&mut SeatStore> {
        let Self {
            active: _,
            first,
            others,
        } = self;

        if let &mut Some((existing, ref mut value)) = first
            && seat == existing
        {
            Some(value)
        } else {
            others.get_mut(&seat)
        }
    }

    fn remove(&mut self, seat: u32) {
        let Self {
            active,
            first,
            others,
        } = self;

        if let &Some((existing, _)) = &*first
            && seat == existing
        {
            debug!("Data control device finished for seat {seat}.");
            *first = others
                .keys()
                .next()
                .copied()
                .and_then(|any| others.remove_entry(&any));
        } else if others.remove(&seat).is_some() {
            debug!("Data control device finished for seat {seat}.");
        } else {
            debug!("Trying to remove seat {seat} that does not exist.");
        }
        others.shrink_to_fit();

        if seat == *active {
            *active = first.as_ref().map_or(0, |&(id, _)| id);
        }
    }
}

const IN_TRANSFER_BUFFERS: usize = 4;

#[derive(Default, Debug)]
struct PendingOffers {
    offers: [Option<AutoDestroy<ExtDataControlOfferV1>>; IN_TRANSFER_BUFFERS],
    mimes: [BestMimeTypeFinder<String>; IN_TRANSFER_BUFFERS],
    transfers: [Option<Transfer>; IN_TRANSFER_BUFFERS],
    next: u8,
}

#[derive(Debug)]
struct Transfer {
    read: OwnedFd,
    data: OwnedFd,
    len: u64,

    mime: MimeType,
}

impl PendingOffers {
    fn init(&mut self, offer: ExtDataControlOfferV1) {
        const _: () = assert!(IN_TRANSFER_BUFFERS.is_power_of_two());

        let Self {
            offers,
            mimes,
            transfers,
            next,
        } = self;

        let idx = usize::from(*next) & (IN_TRANSFER_BUFFERS - 1);
        if let Some(id) = &offers[idx] {
            warn!("Dropping old offer for peer {idx}: {:?}", id.id());
        }

        offers[idx] = Some(AutoDestroy(offer));
        mimes[idx] = BestMimeTypeFinder::default();
        transfers[idx] = None;

        *next = next.wrapping_add(1);
    }

    fn add_mime(&mut self, offer: &ExtDataControlOfferV1, mime: String) {
        let Ok(mime_type) = MimeType::from(&mime) else {
            warn!("Mime {mime:?} too long, ignoring.");
            return;
        };
        let Some(idx) = self.find(offer) else {
            warn!(
                "Trying to add mime to offer that does not exist: {:?}",
                offer.id()
            );
            return;
        };

        self.mimes[idx].add_mime(&mime_type, mime);
    }

    fn start_transfer(
        &mut self,
        tmp_file_unsupported: &mut bool,
        epoll: impl AsFd,
        offer: &ExtDataControlOfferV1,
    ) -> Result<(), CliError> {
        let Some(idx) = self.find(offer) else {
            error!(
                "Failed to start transfer for offer that does not exist: {:?}",
                offer.id()
            );
            return Ok(());
        };

        self.start_transfer_(tmp_file_unsupported, epoll, idx)
    }

    fn start_transfer_(
        &mut self,
        tmp_file_unsupported: &mut bool,
        epoll: impl AsFd,
        idx: usize,
    ) -> Result<(), CliError> {
        let Some(mime) = self.mimes[idx].pop_best() else {
            warn!("No usable mimes returned, dropping offer.");
            self.reset(idx);
            return Ok(());
        };

        info!("Starting transfer for peer {idx} of mime {mime:?}.");
        let mime_type = MimeType::from(&mime).unwrap();

        let data = if is_plaintext_mime(&mime) {
            memfd_create(c"ringboard_wayland_copy", MemfdFlags::empty())
                .map_io_err(|| "Failed to create copy file.")?
        } else {
            create_tmp_file(
                tmp_file_unsupported,
                CWD,
                c".",
                c".ringboard-wayland-scratchpad",
                OFlags::RDWR,
                Mode::empty(),
            )
            .map_io_err(|| "Failed to create copy temp file.")?
        };

        let (read, write) = pipe().map_io_err(|| "Failed to create pipe.")?;
        self.offers[idx]
            .as_ref()
            .unwrap()
            .receive(mime, write.as_fd());

        epoll::add(
            epoll,
            &read,
            epoll::EventData::new_u64(u64::try_from(idx).unwrap()),
            epoll::EventFlags::IN,
        )
        .map_io_err(|| "Failed to register epoll interest in read end of data transfer pipe.")?;
        self.transfers[idx] = Some(Transfer {
            read,
            data,
            len: 0,
            mime: mime_type,
        });

        Ok(())
    }

    fn continue_transfer(
        &mut self,
        tmp_file_unsupported: &mut bool,
        server: impl AsFd,
        epoll: impl AsFd,
        deduplicator: &mut CopyDeduplication,
        idx: usize,
    ) -> Result<(), CliError> {
        let Some(Transfer {
            read,
            data,
            len,
            mime,
        }) = &mut self.transfers[idx]
        else {
            error!("Received poll notification for non-existent peer: {idx}.");
            return Ok(());
        };

        {
            let log_bytes_received = |count| trace!("Received {count} bytes from peer {idx}.");

            let mut total = 0;
            loop {
                match {
                    let max_remaining = usize::MAX / 2 - usize::try_from(*len).unwrap();
                    splice(
                        &read,
                        None,
                        &data,
                        Some(len),
                        max_remaining,
                        if total == 0 {
                            SpliceFlags::empty()
                        } else {
                            SpliceFlags::NONBLOCK
                        },
                    )
                } {
                    Err(Errno::AGAIN) => {
                        log_bytes_received(total);
                        return Ok(());
                    }
                    r => {
                        let count =
                            r.map_io_err(|| "Failed to splice data from peer into transfer file.")?;
                        log_bytes_received(count);
                        if count == 0 {
                            break;
                        }
                        total += count;
                    }
                }
            }
        }
        let len = *len;
        debug!("Finished transferring {len} bytes from peer {idx}.");

        let mmap;
        if len == 0 || {
            mmap = Mmap::new(&data, usize::try_from(len).unwrap())
                .map_io_err(|| "Failed to mmap copy file")?;
            debug_assert_eq!(mmap.len(), usize::try_from(len).unwrap());
            mmap.iter().all(u8::is_ascii_whitespace)
        } {
            warn!("Dropping empty or blank selection for peer {idx} on mime {mime:?}.");
            self.start_transfer_(tmp_file_unsupported, epoll, idx)?;
            return Ok(());
        }

        let data_hash = CopyDeduplication::hash(CopyData::Slice(&mmap), len);
        if let Some(existing) = deduplicator.check(data_hash, CopyData::Slice(&mmap)) {
            info!("Promoting duplicate entry from peer {idx} on mime {mime:?} to front.");
            if let MoveToFrontResponse::Success { id } =
                MoveToFrontRequest::response(&server, existing, None)?
            {
                deduplicator.remember(data_hash, id);
                self.reset(idx);
                return Ok(());
            }
        }

        let AddResponse::Success { id } =
            AddRequest::response_add_unchecked(&server, RingKind::Main, mime, data)?;
        deduplicator.remember(data_hash, id);
        info!("Transfer for peer {idx} on mime {mime:?} complete.");
        self.reset(idx);

        Ok(())
    }

    fn consume(&mut self, offer: &ExtDataControlOfferV1) {
        let Some(idx) = self.find(offer) else {
            error!(
                "Failed to consume offer that does not exist: {:?}",
                offer.id()
            );
            return;
        };
        self.reset(idx);
    }

    fn reset(&mut self, idx: usize) {
        let Self {
            offers,
            mimes,
            transfers,
            next: _,
        } = self;

        offers[idx].take();
        mem::take(&mut mimes[idx]);
        transfers[idx].take();
    }

    fn find(&self, offer: &ExtDataControlOfferV1) -> Option<usize> {
        self.offers
            .iter()
            .position(|id| id.as_ref().map(|id| id.id()) == Some(offer.id()))
    }
}

#[derive(Default, Debug)]
struct AppDefault {
    manager: Option<AutoDestroy<ExtDataControlManagerV1>>,
    virtual_keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    foreign_toplevels: Option<AutoDestroy<ExtForeignToplevelListV1>>,
    seats: Seats,
    pending_offers: PendingOffers,

    sources: Sources,
    outgoing_transfers: OutgoingTransfers,
    pending_paste: bool,

    tmp_file_unsupported: bool,

    error: Option<CliError>,
}

#[derive(Debug)]
struct App {
    inner: AppDefault,
    epoll: OwnedFd,
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        this: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wl_registry::Event;

        fn singleton<T: Proxy + 'static, U>(
            registry: &WlRegistry,
            qh: &QueueHandle<App>,
            object: &mut Option<U>,
            map: impl FnOnce(T) -> U,
            error: &mut Option<CliError>,
            event: &Event,
        ) where
            App: Dispatch<T, ()>,
        {
            if let &Event::Global {
                name,
                ref interface,
                version,
            } = event
                && interface == T::interface().name
            {
                if object.is_some() {
                    *error = Some(CliError::BadWaylandGlobal {
                        message: "duplicate global found",
                        interface: T::interface().name,
                    });
                } else {
                    let interface = registry.bind(name, version, qh, ());
                    *object = Some(map(interface));
                }
            }
        }

        trace!("Registry event: {event:?}");
        singleton(
            registry,
            qh,
            &mut this.inner.manager,
            AutoDestroy,
            &mut this.inner.error,
            &event,
        );
        singleton(
            registry,
            qh,
            &mut this.inner.virtual_keyboard_manager,
            identity,
            &mut this.inner.error,
            &event,
        );
        singleton(
            registry,
            qh,
            &mut this.inner.foreign_toplevels,
            AutoDestroy,
            &mut this.inner.error,
            &event,
        );
        match event {
            Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == WlSeat::interface().name {
                    let _: WlSeat = registry.bind(name, version, qh, name);
                }
            }
            Event::GlobalRemove { name } => this.inner.seats.remove(name),
            _ => debug_assert!(false, "Unhandled registry event: {event:?}"),
        }
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        event: <ExtDataControlManagerV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug_assert!(false, "Unhandled data control manager event: {event:?}");
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        event: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug_assert!(false, "Unhandled virtual keyboard manager event: {event:?}");
    }
}

impl Dispatch<WlSeat, u32> for App {
    fn event(
        this: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        &id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wl_seat::Event;
        trace!("Seat event: {event:?}");
        match event {
            Event::Name { name: _ } => {
                if let Some(manager) = &this.inner.manager {
                    let device = manager.get_data_device(seat, qh, id);
                    let keyboard = seat.get_keyboard(qh, id);
                    this.inner.seats.add(id, seat.clone(), device, keyboard);
                    debug!("Listening for clipboard events on seat {id}.");
                }
            }
            Event::Capabilities { capabilities: _ } => (),
            _ => debug_assert!(false, "Unhandled seat event: {event:?}"),
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, u32> for App {
    fn event(
        this: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as Proxy>::Event,
        &seat: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let run = || {
            use ext_data_control_device_v1::Event;
            match event {
                Event::DataOffer { id } => {
                    trace!("Received data offer event: {:?}", id.id());
                    this.inner.pending_offers.init(id);
                }
                Event::Selection { id } => {
                    debug!(
                        "Received selection event: {:?}",
                        id.as_ref().map(wayland_client::Proxy::id)
                    );
                    let Some(id) = id else { return Ok(()) };
                    if this.inner.sources.open[1].is_some() {
                        debug!("Ignoring self selection.");
                        this.inner.pending_offers.consume(&id);
                    } else {
                        this.inner.pending_offers.start_transfer(
                            &mut this.inner.tmp_file_unsupported,
                            &this.epoll,
                            &id,
                        )?;
                    }
                }
                Event::PrimarySelection { id } => {
                    trace!(
                        "Received primary selection event: {:?}",
                        id.as_ref().map(wayland_client::Proxy::id)
                    );
                    let Some(id) = id else { return Ok(()) };
                    this.inner.pending_offers.consume(&id);
                }
                Event::Finished => this.inner.seats.remove(seat),
                _ => debug_assert!(false, "Unhandled data control device event: {event:?}"),
            }
            Ok(())
        };

        let err = run().err();
        if this.inner.error.is_none() {
            this.inner.error = err;
        }
    }

    event_created_child!(Self, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for App {
    fn event(
        this: &mut Self,
        id: &ExtDataControlOfferV1,
        event: <ExtDataControlOfferV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_offer_v1::Event;
        match event {
            Event::Offer { mime_type } => {
                trace!(
                    "Received mime type offer for id {:?}: {mime_type:?}",
                    id.id()
                );
                this.inner.pending_offers.add_mime(id, mime_type);
            }
            _ => debug_assert!(false, "Unhandled data control offer event: {event:?}"),
        }
    }
}

#[derive(Default, Debug)]
struct Sources {
    mime: MimeType,
    fd: Option<MaybeRc<OwnedFd>>,
    len: usize,
    open: [Option<AutoDestroy<ExtDataControlSourceV1>>; 2],
}

const OUT_TRANSFER_BUFFERS: usize = 4;

#[derive(Default, Debug)]
struct OutgoingTransfers {
    transfers: [Option<OutgoingTransfer>; OUT_TRANSFER_BUFFERS],
    next: u8,
}

#[derive(Debug)]
struct MaybeRc<T> {
    rc: Option<Rc<T>>,
    raw: Option<T>,
}

impl<T> MaybeRc<T> {
    const fn new(t: T) -> Self {
        Self {
            rc: None,
            raw: Some(t),
        }
    }

    fn convert_rc(&mut self) -> Rc<T> {
        let Self { rc, raw } = self;
        match (&rc, raw.take()) {
            (Some(rc), None) => rc.clone(),
            (None, Some(raw)) => {
                let new = Rc::new(raw);
                *rc = Some(new.clone());
                new
            }
            (Some(_), Some(_)) | (None, None) => unreachable!(),
        }
    }
}

impl<T> Deref for MaybeRc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        let Self { rc, raw } = self;
        match (rc, raw) {
            (Some(rc), None) => rc,
            (None, Some(raw)) => raw,
            (Some(_), Some(_)) | (None, None) => unreachable!(),
        }
    }
}

impl OutgoingTransfers {
    fn begin(
        &mut self,
        epoll: impl AsFd,
        data: &mut MaybeRc<OwnedFd>,
        data_len: usize,
        write: OwnedFd,
    ) -> Result<(), CliError> {
        const _: () = assert!(OUT_TRANSFER_BUFFERS.is_power_of_two());
        debug!("Starting transfer of {data_len} bytes.");

        let mut offset = 0;
        if Self::transfer(&**data, &write, &mut offset, data_len)? {
            info!("Fast path paste completed.");
            return Ok(());
        }

        let Self { transfers, next } = self;

        let idx = usize::from(*next) & (OUT_TRANSFER_BUFFERS - 1);
        if transfers[idx].is_some() {
            warn!("Dropping old outgoing transfer for peer {idx}.");
        }

        epoll::add(
            epoll,
            &write,
            epoll::EventData::new_u64(u64::try_from(IN_TRANSFER_BUFFERS + idx).unwrap()),
            epoll::EventFlags::OUT,
        )
        .map_io_err(|| {
            "Failed to register epoll interest in write end of outgoing transfer pipe."
        })?;
        transfers[idx] = Some(OutgoingTransfer {
            data: data.convert_rc(),
            write,
            offset,
            total: data_len,
        });

        *next = next.wrapping_add(1);

        Ok(())
    }

    fn continue_transfer(&mut self, idx: usize) -> Result<(), CliError> {
        let Some(OutgoingTransfer {
            ref data,
            ref write,
            ref mut offset,
            total,
        }) = self.transfers[idx]
        else {
            error!("Received poll notification for non-existent transfer peer: {idx}.");
            return Ok(());
        };
        debug!("Continuing transfer to peer {idx}.");

        if Self::transfer(data, write, offset, total)? {
            info!("Finished transfer to peer {idx}.");
            self.transfers[idx].take();
        }
        Ok(())
    }

    fn transfer(
        data: impl AsFd,
        write: impl AsFd,
        offset: &mut u64,
        total: usize,
    ) -> Result<bool, CliError> {
        loop {
            let remaining = total - usize::try_from(*offset).unwrap();
            match splice(
                &data,
                Some(offset),
                &write,
                None,
                remaining,
                SpliceFlags::NONBLOCK,
            ) {
                Err(Errno::AGAIN) => return Ok(false),
                Err(Errno::PIPE) => return Ok(true),
                Err(Errno::INVAL) => {
                    let bytes = io::copy(
                        &mut *ManuallyDrop::new(unsafe {
                            File::from_raw_fd(data.as_fd().as_raw_fd())
                        }),
                        &mut *ManuallyDrop::new(unsafe {
                            File::from_raw_fd(write.as_fd().as_raw_fd())
                        }),
                    )
                    .map_io_err(|| "Fallback paste into peer failed.")?;
                    debug!("Fallback finished sending {bytes} bytes.");
                    return Ok(true);
                }
                r => {
                    let count =
                        r.map_io_err(|| "Failed to splice data from data file into peer.")?;
                    trace!("Sent {count} bytes.");
                    if u64::try_from(total).unwrap() == *offset || count == 0 {
                        debug!("Finished sending {offset} bytes.");
                        return Ok(true);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct OutgoingTransfer {
    data: Rc<OwnedFd>,
    write: OwnedFd,
    offset: u64,
    total: usize,
}

fn handle_paste_event(
    paste_socket: impl AsFd,
    ancillary_buf: &mut [MaybeUninit<u8>; rustix::cmsg_space!(ScmRights(1))],

    qh: &QueueHandle<App>,
    manager: Option<&AutoDestroy<ExtDataControlManagerV1>>,
    seats: &Seats,
    auto_paste: bool,
    pending_paste: &mut bool,
    sources: &mut Sources,

    server: impl AsFd,
    deduplicator: &mut CopyDeduplication,
) -> Result<(), CliError> {
    struct MoveToFrontGuard<'a, Server: AsFd>(Server, Option<Mmap>, &'a mut CopyDeduplication);

    impl<Server: AsFd> Drop for MoveToFrontGuard<'_, Server> {
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
            let Some(data) = &self.1 else {
                return;
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
    let guard = MoveToFrontGuard(
        server,
        if let Some(fd) = &fd {
            Some(Mmap::from(fd).map_io_err(|| "Failed to mmap paste file.")?)
        } else {
            None
        },
        deduplicator,
    );
    if let Some(data) = &guard.1 {
        debug!("Paste file is {} bytes long.", data.len());
    }

    let Some(manager) = manager else {
        debug!("No manager for paste.");
        return Ok(());
    };
    let Some((_, device, _, _)) = seats.get(seats.active) else {
        warn!("Received paste command with no seats to paste into, ignoring.");
        return Ok(());
    };

    let Some(fd) = fd else {
        info!("Clearing selections.");
        device.set_primary_selection(None);
        device.set_selection(None);
        return Ok(());
    };

    let Sources {
        mime: mime_,
        fd: fd_,
        len,
        open,
    } = sources;
    *mime_ = mime;
    *fd_ = Some(MaybeRc::new(fd));
    *len = guard.1.as_ref().map_or(0, Mmap::len);

    let supported_mimes = generate_supported_mimes(&mime);
    trace!("Offering mimes: {supported_mimes:?}");
    for (i, slot) in open.iter_mut().enumerate() {
        let source = AutoDestroy(manager.create_data_source(qh, i));
        for mime in &supported_mimes {
            source.offer((*mime).to_string());
        }
        match i {
            0 => device.set_primary_selection(Some(&source)),
            1 => device.set_selection(Some(&source)),
            _ => unreachable!(),
        }
        *slot = Some(source);
    }
    info!("Claimed selection ownership.");

    *pending_paste = auto_paste && trigger_paste;

    Ok(())
}

fn generate_supported_mimes(mime: &str) -> ArrayVec<&str, 8> {
    let mut supported_mimes = ArrayVec::new_const();
    if !mime.is_empty() {
        supported_mimes.push(mime);
    }
    if is_text_mime(mime) {
        supported_mimes
            .try_extend_from_slice(&[
                "UTF8_STRING",
                "TEXT",
                "STRING",
                "text/plain",
                "text/plain;charset=utf-8",
                "text/plain;charset=us-ascii",
                "text/plain;charset=unicode",
            ])
            .unwrap();
    }
    supported_mimes
}

impl Dispatch<ExtDataControlSourceV1, usize> for App {
    fn event(
        this: &mut Self,
        _: &ExtDataControlSourceV1,
        event: <ExtDataControlSourceV1 as Proxy>::Event,
        &id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_source_v1::Event;

        let Sources {
            mime,
            fd: data,
            len,
            open,
        } = &mut this.inner.sources;
        match event {
            Event::Send { mime_type, fd } => {
                if !generate_supported_mimes(mime).contains(&mime_type.as_str()) {
                    debug!("Rejecting transfer for mime that was not offered: {mime_type:?}");
                    return;
                }
                let Some(data) = data else {
                    debug!("Possible bug? No data available, but transfer was requested.");
                    return;
                };

                let err = this
                    .inner
                    .outgoing_transfers
                    .begin(&this.epoll, data, *len, fd)
                    .err();
                if this.inner.error.is_none() {
                    this.inner.error = err;
                }
            }
            Event::Cancelled => {
                debug!("Releasing ownership of {} selection.", match id {
                    0 => "primary",
                    1 => "clipboard",
                    _ => unreachable!(),
                });
                open[id].take();
                if open.iter().all(Option::is_none) {
                    data.take();
                }
            }
            _ => debug_assert!(false, "Unhandled data control source event: {event:?}"),
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        event: <ZwpVirtualKeyboardV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug_assert!(false, "Unhandled virtual keyboard event: {event:?}");
    }
}

impl Dispatch<WlKeyboard, u32> for App {
    fn event(
        this: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as Proxy>::Event,
        &seat: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_keyboard::Event;

        trace!("Keyboard event: {event:?}");
        if let Event::Keymap { format, fd, size } = event {
            let Some((seat, _, _, virtual_keyboard)) = this.inner.seats.get_mut(seat) else {
                error!("Received keyboard event for seat {seat} that does not exist.");
                return;
            };
            let Some(ref manager) = this.inner.virtual_keyboard_manager else {
                debug!("Trying to set keymap with no virtual keyboard manager present.");
                return;
            };

            let keyboard = virtual_keyboard
                .get_or_insert_with(|| AutoDestroy(manager.create_virtual_keyboard(seat, qh, ())));
            keyboard.keymap(format.into(), fd.as_fd(), size);
        }
        this.inner.seats.active = seat;
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for App {
    fn event(
        this: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: <ExtForeignToplevelListV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::Event;

        match event {
            Event::Toplevel { toplevel } => trace!("New foreign top level: {:?}", toplevel.id()),
            Event::Finished => {
                trace!("Unsubscribing from toplevel events.");
                this.inner.foreign_toplevels.take();
            }
            _ => debug_assert!(false, "Unhandled foreign top level list event: {event:?}"),
        }
    }

    event_created_child!(Self, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for App {
    fn event(
        this: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: <ExtForeignToplevelHandleV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event;

        trace!("Foreign top level handle event: {event:?}");
        if this.inner.pending_paste
            && matches!(event, Event::Done | Event::Closed)
            && let Some((_, _, _, Some(keyboard))) = &this.inner.seats.get(this.inner.seats.active)
        {
            // Shift modifier + Insert key
            keyboard.modifiers(1, 0, 0, 0);
            keyboard.key(1, 110, KeyState::Pressed.into());
            keyboard.key(2, 110, KeyState::Released.into());
            keyboard.modifiers(0, 0, 0, 0);
            info!("Sent paste command.");

            this.inner.pending_paste = false;
        }
        if matches!(event, Event::Closed) {
            handle.destroy();
        }
    }
}
