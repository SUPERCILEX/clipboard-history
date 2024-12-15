#![feature(let_chains)]

use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    hash::BuildHasherDefault,
    io::ErrorKind::WouldBlock,
    mem,
    mem::ManuallyDrop,
    ops::Deref,
    os::fd::{AsFd, OwnedFd},
    ptr,
    rc::Rc,
};

use arrayvec::ArrayVec;
use error_stack::Report;
use log::{debug, error, info, trace, warn};
use ringboard_sdk::{
    api::{AddRequest, MoveToFrontRequest, PasteCommand, connect_to_server},
    core::{
        Error, IoErr, create_tmp_file,
        dirs::{paste_socket_file, socket_file},
        init_unix_server, is_plaintext_mime,
        protocol::{AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, RingKind},
        ring::Mmap,
    },
    is_text_mime,
};
use ringboard_watcher_utils::{
    best_target::BestMimeTypeFinder,
    deduplication::{CopyData, CopyDeduplication},
    utils::read_paste_command,
};
use rustc_hash::FxHasher;
use rustix::{
    event::epoll,
    fs::{CWD, MemfdFlags, Mode, OFlags, memfd_create},
    io::Errno,
    net::{SocketAddrUnix, SocketType},
    pipe::{SpliceFlags, pipe, splice},
};
use smallvec::SmallVec;
use thiserror::Error;
use wayland_client::{
    ConnectError, Connection, Dispatch, DispatchError, Proxy, QueueHandle,
    backend::WaylandError,
    event_created_child,
    protocol::{wl_registry, wl_registry::WlRegistry, wl_seat, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
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
        CliError::WaylandConnection(e) => Report::new(e).change_context(wrapper),
        CliError::WaylandDispatch(e) => Report::new(e).change_context(wrapper),
        CliError::BadWaylandGlobal {
            message: _,
            interface: _,
        } => Report::new(wrapper),
    }
}

fn run() -> Result<(), CliError> {
    info!(
        "Starting Ringboard Wayland clipboard listener v{}.",
        env!("CARGO_PKG_VERSION")
    );

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

    let mut ancillary_buf = [0; rustix::cmsg_space!(ScmRights(1))];

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
            interface: "zwlr_data_control_manager_v1",
        });
    };
    debug!("Wayland globals initialized.");

    let mut epoll_events = epoll::EventVec::with_capacity(4);

    let mut deduplicator = CopyDeduplication::new()?;

    info!("Starting event loop.");
    loop {
        if let Some(e) = app.inner.error {
            return Err(e);
        }
        if let Some(manager) = &app.inner.manager {
            for (name, seat) in app.inner.pending_seats.drain(..) {
                let device = manager.get_data_device(&seat, &qh, name);
                app.inner.seats.add(name, seat.into_inner(), device);
                debug!("Listening for clipboard events on seat {name}.");
            }
        }
        event_queue.flush().map_err(DispatchError::from)?;

        trace!("Waiting for event.");
        match epoll::wait(&app.epoll, &mut epoll_events, -1) {
            Err(Errno::INTR) => continue,
            r => r.map_io_err(|| "Failed to wait for epoll events.")?,
        };
        for epoll::Event { flags: _, data } in &epoll_events {
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
                    &mut app.inner.sources,
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

impl Destroyable for ZwlrDataControlManagerV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ZwlrDataControlDeviceV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ZwlrDataControlOfferV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

impl Destroyable for ZwlrDataControlSourceV1 {
    fn destroy(&self) {
        self.destroy();
    }
}

struct AutoDestroy<T: Destroyable>(T);

impl<T: Destroyable> AutoDestroy<T> {
    fn into_inner(self) -> T {
        let this = ManuallyDrop::new(self);
        unsafe { ptr::read(&this.0) }
    }
}

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

#[derive(Default, Debug)]
struct Seats {
    first: Option<(
        u32,
        (AutoDestroy<WlSeat>, AutoDestroy<ZwlrDataControlDeviceV1>),
    )>,
    others: HashMap<
        u32,
        (AutoDestroy<WlSeat>, AutoDestroy<ZwlrDataControlDeviceV1>),
        BuildHasherDefault<FxHasher>,
    >,
}

impl Seats {
    fn add(&mut self, seat: u32, seat_obj: WlSeat, device: ZwlrDataControlDeviceV1) {
        let Self { first, others } = self;

        let value = (AutoDestroy(seat_obj), AutoDestroy(device));
        if first.is_none() {
            *first = Some((seat, value));
        } else if others.insert(seat, value).is_some() {
            error!("Duplicate seat: {seat}");
        }
    }

    fn remove(&mut self, seat: u32) {
        let Self { first, others } = self;

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
    }
}

const IN_TRANSFER_BUFFERS: usize = 4;

#[derive(Default, Debug)]
struct PendingOffers {
    offers: [Option<AutoDestroy<ZwlrDataControlOfferV1>>; IN_TRANSFER_BUFFERS],
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
    fn init(&mut self, offer: ZwlrDataControlOfferV1) {
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

    fn add_mime(&mut self, offer: &ZwlrDataControlOfferV1, mime: String) {
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
        offer: &ZwlrDataControlOfferV1,
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
            AddRequest::response_add_unchecked(&server, RingKind::Main, *mime, data)?;
        deduplicator.remember(data_hash, id);
        info!("Transfer for peer {idx} on mime {mime:?} complete.");
        self.reset(idx);

        Ok(())
    }

    fn consume(&mut self, offer: &ZwlrDataControlOfferV1) {
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

    fn find(&self, offer: &ZwlrDataControlOfferV1) -> Option<usize> {
        self.offers
            .iter()
            .position(|id| id.as_ref().map(|id| id.id()) == Some(offer.id()))
    }
}

#[derive(Default, Debug)]
struct AppDefault {
    manager: Option<AutoDestroy<ZwlrDataControlManagerV1>>,
    seats: Seats,
    pending_offers: PendingOffers,

    sources: Sources,
    outgoing_transfers: OutgoingTransfers,

    pending_seats: SmallVec<(u32, AutoDestroy<WlSeat>), 1>,
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

        fn singleton<T: Destroyable + Proxy + 'static>(
            registry: &WlRegistry,
            qhandle: &QueueHandle<App>,
            object: &mut Option<AutoDestroy<T>>,
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
                    let interface = registry.bind(name, version, qhandle, ());
                    *object = Some(AutoDestroy(interface));
                }
            }
        }

        trace!("Registry event: {event:?}");
        singleton(
            registry,
            qh,
            &mut this.inner.manager,
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
                    let seat: WlSeat = registry.bind(name, version, qh, ());
                    this.inner.pending_seats.push((name, AutoDestroy(seat)));
                }
            }
            Event::GlobalRemove { name } => this.inner.seats.remove(name),
            _ => debug_assert!(false, "Unhandled registry event: {event:?}"),
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        event: <ZwlrDataControlManagerV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        debug_assert!(false, "Unhandled data control manager event: {event:?}");
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_seat::Event;
        match event {
            Event::Capabilities { capabilities: _ } | Event::Name { name: _ } => {}
            _ => debug_assert!(false, "Unhandled seat event: {event:?}"),
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, u32> for App {
    fn event(
        this: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as Proxy>::Event,
        &seat: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let run = || {
            use zwlr_data_control_device_v1::Event;
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

    event_created_child!(Self, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for App {
    fn event(
        this: &mut Self,
        id: &ZwlrDataControlOfferV1,
        event: <ZwlrDataControlOfferV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_offer_v1::Event;
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
    open: [Option<AutoDestroy<ZwlrDataControlSourceV1>>; 2],
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
        write: OwnedFd,
    ) -> Result<(), CliError> {
        const _: () = assert!(OUT_TRANSFER_BUFFERS.is_power_of_two());

        let mut len = 0;
        if Self::transfer(&**data, &write, &mut len)? {
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
            len,
        });

        *next = next.wrapping_add(1);

        Ok(())
    }

    fn continue_transfer(&mut self, idx: usize) -> Result<(), CliError> {
        let Some(OutgoingTransfer { data, write, len }) = &mut self.transfers[idx] else {
            error!("Received poll notification for non-existent transfer peer: {idx}.");
            return Ok(());
        };
        debug!("Continuing transfer to peer {idx}.");

        if Self::transfer(data, write, len)? {
            info!("Finished transfer to peer {idx}.");
            self.transfers[idx].take();
        }
        Ok(())
    }

    fn transfer(data: impl AsFd, write: impl AsFd, len: &mut u64) -> Result<bool, CliError> {
        loop {
            let max_remaining = usize::MAX / 2 - usize::try_from(*len).unwrap();
            match splice(
                &data,
                Some(len),
                &write,
                None,
                max_remaining,
                SpliceFlags::NONBLOCK,
            ) {
                Err(Errno::AGAIN) => {
                    return Ok(false);
                }
                r => {
                    let count =
                        r.map_io_err(|| "Failed to splice data from data file into peer.")?;
                    trace!("Sent {count} bytes.");
                    if count == 0 {
                        debug!("Finished sending {len} bytes.");
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
    len: u64,
}

fn handle_paste_event(
    paste_socket: impl AsFd,
    ancillary_buf: &mut [u8; rustix::cmsg_space!(ScmRights(1))],

    qh: &QueueHandle<App>,
    manager: Option<&AutoDestroy<ZwlrDataControlManagerV1>>,
    seats: &Seats,
    sources: &mut Sources,
) -> Result<(), CliError> {
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

    let Some(manager) = manager else {
        debug!("No manager for paste.");
        return Ok(());
    };
    // TODO at some point we should handle multiple seats, but eh for now
    let Some((_, (_, device))) = seats.first.as_ref() else {
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
        open,
    } = sources;
    *mime_ = mime;
    *fd_ = Some(MaybeRc::new(fd));

    let supported_mimes = generate_supported_mimes(&mime);
    trace!("Offering mimes: {supported_mimes:?}");
    for (i, slot) in open.iter_mut().enumerate() {
        let source = AutoDestroy(manager.create_data_source(qh, i));
        for mime in &supported_mimes {
            source.offer(mime.to_string());
        }
        match i {
            0 => device.set_primary_selection(Some(&source)),
            1 => device.set_selection(Some(&source)),
            _ => unreachable!(),
        }
        *slot = Some(source);
    }
    info!("Claimed selection ownership.");

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

impl Dispatch<ZwlrDataControlSourceV1, usize> for App {
    fn event(
        this: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: <ZwlrDataControlSourceV1 as Proxy>::Event,
        &id: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_source_v1::Event;

        let Sources {
            mime,
            fd: data,
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
                    .begin(&this.epoll, data, fd)
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
