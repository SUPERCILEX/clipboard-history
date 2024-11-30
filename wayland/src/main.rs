#![feature(let_chains)]

use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    fs::File,
    hash::BuildHasherDefault,
    io,
    io::{Seek, SeekFrom},
    mem,
    os::fd::{AsFd, OwnedFd},
    sync::{
        mpsc,
        mpsc::{Receiver, SyncSender, TrySendError},
    },
    thread,
};

use error_stack::Report;
use log::{debug, error, info, trace, warn};
use ringboard_sdk::{
    api::{AddRequest, connect_to_server},
    core::{
        Error, IoErr, TEXT_MIMES, create_tmp_file,
        dirs::socket_file,
        protocol::{AddResponse, IdNotFoundError, MimeType, RingKind},
        ring::Mmap,
    },
};
use ringboard_watcher_utils::best_target::BestMimeTypeFinder;
use rustc_hash::FxHasher;
use rustix::{
    fs::{CWD, MemfdFlags, Mode, OFlags, memfd_create},
    net::SocketAddrUnix,
    pipe::pipe,
};
use thiserror::Error;
use wayland_client::{
    ConnectError, Connection, Dispatch, DispatchError, Proxy, QueueHandle, event_created_child,
    protocol::{wl_registry, wl_registry::WlRegistry, wl_seat, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{Event, ZwlrDataControlOfferV1},
};

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] Error),
    #[error("{0}")]
    Sdk(#[from] ringboard_sdk::ClientError),
    #[error("{0}")]
    WaylandConnection(#[from] ConnectError),
    #[error("{0}")]
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

    let (error_send, error_recv) = mpsc::sync_channel(0);
    let (server_send, server_recv) = mpsc::sync_channel(0);
    let (copy_send, copy_recv) = mpsc::sync_channel(0);

    thread::spawn({
        let error_send = error_send.clone();
        move || {
            if let Err(e) = ringboard_server_thread(server_recv) {
                let _ = error_send.send(e);
            }
        }
    });
    thread::spawn(move || {
        if let Err(e) = copy_thread(copy_recv, server_send) {
            let _ = error_send.send(e);
        }
    });

    let conn = Connection::connect_to_env()?;
    debug!("Wayland connection established.");

    let mut event_queue = conn.new_event_queue();
    let mut app = {
        let mut init = Init {
            qh: event_queue.handle(),
            manager: None,
        };

        let mut event_queue = conn.new_event_queue();
        conn.display().get_registry(&event_queue.handle(), ());
        event_queue.roundtrip(&mut init)?;

        let Some(manager) = init.manager else {
            return Err(CliError::BadWaylandGlobal {
                message: "compositor does not implement necessary interface",
                interface: "zwlr_data_control_manager_v1",
            });
        };
        let manager = manager?;

        App {
            manager,
            seats: Seats::default(),
            pending_offers: PendingOffers::default(),

            error: None,
            copy: copy_send,
        }
    };
    debug!("Wayland globals initialized.");
    conn.display().get_registry(&event_queue.handle(), ());

    event_queue.roundtrip(&mut app)?;
    loop {
        if let Some(e) = app.error {
            return Err(e);
        }
        if let Ok(e) = error_recv.try_recv() {
            return Err(e);
        }
        event_queue.blocking_dispatch(&mut app)?;
    }
}

#[derive(Debug)]
enum RingboardServerCommand {
    Add { mime_type: MimeType, file: File },
}

fn ringboard_server_thread(recv: Receiver<RingboardServerCommand>) -> Result<(), CliError> {
    let server = {
        let socket_file = socket_file();
        let addr = SocketAddrUnix::new(&socket_file)
            .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?;
        connect_to_server(&addr)?
    };
    debug!("Ringboard connection established.");

    for command in recv {
        debug!("Received command: {command:?}");
        match command {
            RingboardServerCommand::Add { mime_type, file } => {
                // TODO dedup
                let AddResponse::Success { id } =
                    AddRequest::response_add_unchecked(&server, RingKind::Main, mime_type, file)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
enum CopyCommand {
    Copy { mime_type: MimeType, data: OwnedFd },
}

fn copy_thread(
    recv: Receiver<CopyCommand>,
    server: SyncSender<RingboardServerCommand>,
) -> Result<(), CliError> {
    let mut tmp_file_unsupported = false;
    for command in recv {
        debug!("Received command: {command:?}");
        match command {
            CopyCommand::Copy { mime_type, data } => {
                let file = if TEXT_MIMES.contains(&mime_type.as_str()) {
                    memfd_create(c"ringboard_wayland_copy", MemfdFlags::empty())
                        .map_io_err(|| "Failed to create copy file.")?
                } else {
                    create_tmp_file(
                        &mut tmp_file_unsupported,
                        CWD,
                        c".",
                        c".ringboard-wayland-scratchpad",
                        OFlags::RDWR,
                        Mode::empty(),
                    )
                    .map_io_err(|| "Failed to create copy temp file.")?
                };
                let mut file = File::from(file);

                let len = io::copy(&mut File::from(data), &mut file)
                    .map_io_err(|| "Failed to copy from wayland peer to copy file.")?;
                if len == 0
                    || Mmap::new(&file, usize::try_from(len).unwrap())
                        .map_io_err(|| "Failed to mmap copy file")?
                        .iter()
                        .all(u8::is_ascii_whitespace)
                {
                    // TODO consider handling Chrome being dumb and returning an empty buffer for
                    //  text when a chromium/ mime is available
                    warn!("Dropping empty or blank selection.");
                    continue;
                }
                file.seek(SeekFrom::Start(0))
                    .map_io_err(|| "Failed to reset copy file offset.")?;

                let _ = server.send(RingboardServerCommand::Add { mime_type, file });
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Init {
    qh: QueueHandle<App>,
    manager: Option<Result<ZwlrDataControlManagerV1, CliError>>,
}

impl Dispatch<WlRegistry, ()> for Init {
    fn event(
        Self { qh, manager }: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_registry::Event;

        fn singleton<T: Proxy + 'static>(
            registry: &WlRegistry,
            qhandle: &QueueHandle<App>,
            object: &mut Option<Result<T, CliError>>,
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
                if let Some(Ok(_)) = object {
                    *object = Some(Err(CliError::BadWaylandGlobal {
                        message: "duplicate global found",
                        interface: T::interface().name,
                    }));
                } else if object.is_none() {
                    let interface = registry.bind(name, version, qhandle, ());
                    *object = Some(Ok(interface));
                }
            }
        }

        singleton(registry, qh, manager, &event);
    }
}

struct ZwlrDataControlDeviceV1AutoDestroy(ZwlrDataControlDeviceV1);

impl Debug for ZwlrDataControlDeviceV1AutoDestroy {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Drop for ZwlrDataControlDeviceV1AutoDestroy {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

#[derive(Default, Debug)]
struct Seats {
    first: Option<(u32, ZwlrDataControlDeviceV1AutoDestroy)>,
    others: HashMap<u32, ZwlrDataControlDeviceV1AutoDestroy, BuildHasherDefault<FxHasher>>,
}

impl Seats {
    fn add(&mut self, seat: u32, device: ZwlrDataControlDeviceV1) {
        let device = ZwlrDataControlDeviceV1AutoDestroy(device);
        if self.first.is_none() {
            self.first = Some((seat, device));
        } else if self.others.insert(seat, device).is_some() {
            error!("Duplicate seat: {seat}");
        }
    }

    fn remove(&mut self, seat: u32) {
        if let &Some((existing, _)) = &self.first
            && seat == existing
        {
            debug!("Data control device finished for seat {seat}.");
            self.first = self
                .others
                .keys()
                .next()
                .copied()
                .and_then(|any| self.others.remove_entry(&any));
        } else if self.others.remove(&seat).is_some() {
            debug!("Data control device finished for seat {seat}.");
        } else {
            debug!("Trying to remove seat {seat} that does not exist.");
        }
        self.others.shrink_to_fit();
    }
}

struct ZwlrDataControlOfferV1AutoDestroy(ZwlrDataControlOfferV1);

impl Debug for ZwlrDataControlOfferV1AutoDestroy {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Drop for ZwlrDataControlOfferV1AutoDestroy {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

const OFFER_BUFFERS: usize = 4;

#[derive(Default, Debug)]
struct PendingOffers {
    ids: [Option<ZwlrDataControlOfferV1AutoDestroy>; OFFER_BUFFERS],
    offers: [BestMimeTypeFinder<String>; OFFER_BUFFERS],
    next: u8,
}

impl PendingOffers {
    fn init(&mut self, offer: ZwlrDataControlOfferV1) {
        const _: () = assert!(OFFER_BUFFERS.is_power_of_two());

        let idx = usize::from(self.next) & (OFFER_BUFFERS - 1);
        if let Some(id) = &self.ids[idx] {
            warn!("Dropping old offer: {:?}", id.0.id());
        }

        self.ids[idx] = Some(ZwlrDataControlOfferV1AutoDestroy(offer));
        self.offers[idx] = BestMimeTypeFinder::default();

        self.next = self.next.wrapping_add(1);
    }

    fn add_mime(&mut self, offer: &ZwlrDataControlOfferV1, mime: String) {
        let Ok(mime_type) = MimeType::from(mime.as_str()) else {
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

        self.offers[idx].add_mime(&mime_type, mime);
    }

    fn consume(
        &mut self,
        offer: &ZwlrDataControlOfferV1,
    ) -> Option<(
        ZwlrDataControlOfferV1AutoDestroy,
        BestMimeTypeFinder<String>,
    )> {
        let Some(idx) = self.find(offer) else {
            error!("Failed to copy offer that does not exist: {:?}", offer.id());
            return None;
        };

        Some((
            self.ids[idx].take().unwrap(),
            mem::take(&mut self.offers[idx]),
        ))
    }

    fn find(&self, offer: &ZwlrDataControlOfferV1) -> Option<usize> {
        self.ids
            .iter()
            .position(|id| id.as_ref().map(|id| id.0.id()) == Some(offer.id()))
    }
}

#[derive(Debug)]
struct App {
    manager: ZwlrDataControlManagerV1,
    seats: Seats,
    pending_offers: PendingOffers,

    error: Option<CliError>,
    copy: SyncSender<CopyCommand>,
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
        trace!("Registry event: {event:?}");
        match event {
            Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == WlSeat::interface().name {
                    let seat: WlSeat = registry.bind(name, version, qh, ());
                    let device = this.manager.get_data_device(&seat, qh, name);
                    this.seats.add(name, device);
                }
            }
            Event::GlobalRemove { name } => this.seats.remove(name),
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
                    this.pending_offers.init(id);
                }
                Event::Selection { id } => {
                    debug!(
                        "Received selection event: {:?}",
                        id.as_ref().map(wayland_client::Proxy::id)
                    );
                    let Some(id) = id else { return Ok(()) };
                    // TODO add info logs everywhere (copy x11)
                    let Some((id_, finder)) = this.pending_offers.consume(&id) else {
                        return Ok(());
                    };
                    debug_assert_eq!(id_.0, id);
                    let Some((mime_id, mime_type)) = finder.best() else {
                        warn!("No usable mimes returned, dropping offer.");
                        return Ok(());
                    };
                    debug_assert_eq!(mime_id, mime_type.as_str());

                    let (read, write) = pipe().map_io_err(|| "Failed to create pipe.")?;
                    id.receive(mime_id, write.as_fd());
                    if let Err(e) = this.copy.try_send(CopyCommand::Copy {
                        mime_type,
                        data: read,
                    }) {
                        let (TrySendError::Full(cmd) | TrySendError::Disconnected(cmd)) = e;
                        warn!("Copy thread busy… creating temporary thread.");
                        // TODO
                    }
                }
                Event::PrimarySelection { id } => {
                    trace!(
                        "Received primary selection event: {:?}",
                        id.as_ref().map(wayland_client::Proxy::id)
                    );
                    let Some(id) = id else { return Ok(()) };
                    this.pending_offers.consume(&id);
                }
                Event::Finished => this.seats.remove(seat),
                _ => debug_assert!(false, "Unhandled data control device event: {event:?}"),
            }
            Ok(())
        };

        this.error = run().err();
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
        match event {
            Event::Offer { mime_type } => {
                trace!(
                    "Received mime type offer for id {:?}: {mime_type:?}",
                    id.id()
                );
                this.pending_offers.add_mime(id, mime_type);
            }
            _ => debug_assert!(false, "Unhandled data control offer event: {event:?}"),
        }
    }
}
