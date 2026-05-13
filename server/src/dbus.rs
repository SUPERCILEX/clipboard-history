#![cfg(feature = "dbus")]

use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::fd::OwnedFd,
    thread::{self, JoinHandle},
};

use log::{info, warn};
use ringboard_core::dirs::{data_dir, socket_file};
use ringboard_core::protocol::{AddResponse, RingKind};
use ringboard_sdk::{
    DatabaseReader,
    api::{AddRequest, MoveToFrontRequest, RemoveRequest, connect_to_server},
};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::net::SocketAddrUnix;
use zbus::connection::Builder;

pub const BUS_NAME: &str = "com.github.SUPERCILEX.Ringboard";
pub const OBJECT_PATH: &str = "/com/github/SUPERCILEX/Ringboard";
pub const INTERFACE_NAME: &str = "com.github.SUPERCILEX.Ringboard1";

// Convention for interface methods:
//
// The SDK is synchronous (rustix syscalls, blocking socket I/O), and the
// tokio runtime hosting zbus is single-threaded. To avoid blocking the
// dispatcher (and timing out heartbeats), interface methods must wrap
// their SDK work inside `tokio::task::spawn_blocking(...).await`. Don't
// .await SDK calls directly inside the method body.

pub fn spawn() -> JoinHandle<()> {
    thread::Builder::new()
        .name("ringboard-dbus".into())
        .spawn(run)
        .expect("failed to spawn ringboard-dbus thread")
}

fn run() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for ringboard-dbus");

    if let Err(e) = rt.block_on(serve()) {
        warn!("ringboard-dbus exiting: {e}");
    }
}

fn open_server() -> zbus::fdo::Result<OwnedFd> {
    let sock = socket_file();
    let addr = SocketAddrUnix::new(&sock)
        .map_err(|e| zbus::fdo::Error::Failed(format!("invalid socket path: {e}")))?;
    connect_to_server(&addr)
        .map_err(|e| zbus::fdo::Error::Failed(format!("connect to server: {e}")))
}

fn payload_memfd(bytes: &[u8]) -> zbus::fdo::Result<File> {
    let fd = memfd_create(c"ringboard-dbus-add", MemfdFlags::CLOEXEC)
        .map_err(|e| zbus::fdo::Error::Failed(format!("memfd_create: {e}")))?;
    let mut file = File::from(fd);
    file.write_all(bytes)
        .map_err(|e| zbus::fdo::Error::Failed(format!("write payload: {e}")))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| zbus::fdo::Error::Failed(format!("seek payload: {e}")))?;
    Ok(file)
}

struct Iface;

#[zbus::interface(name = "com.github.SUPERCILEX.Ringboard1")]
impl Iface {
    /// Drop every entry from the server.
    async fn wipe(&self) -> zbus::fdo::Result<()> {
        tokio::task::spawn_blocking(|| -> zbus::fdo::Result<()> {
            let server = open_server()?;

            let mut database = data_dir();
            let db = DatabaseReader::open(&mut database)
                .map_err(|e| zbus::fdo::Error::Failed(format!("open database: {e}")))?;

            let ids: Vec<u64> = db
                .favorites()
                .chain(db.main())
                .map(|e| e.id())
                .collect();

            for id in ids {
                let _resp = RemoveRequest::response(&server, id)
                    .map_err(|e| zbus::fdo::Error::Failed(format!("remove {id}: {e}")))?;
            }

            Ok(())
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("wipe join: {e}")))??;
        Ok(())
    }

    /// Drop the entry with the given id.
    async fn remove(&self, id: u64) -> zbus::fdo::Result<()> {
        tokio::task::spawn_blocking(move || -> zbus::fdo::Result<()> {
            let server = open_server()?;
            let resp = RemoveRequest::response(&server, id)
                .map_err(|e| zbus::fdo::Error::Failed(format!("remove {id}: {e}")))?;
            if let Some(err) = resp.error {
                return Err(zbus::fdo::Error::Failed(format!("remove {id}: {err:?}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("remove join: {e}")))??;
        Ok(())
    }

    /// Move the entry with the given id to the front of the main ring.
    async fn move_to_front(&self, id: u64) -> zbus::fdo::Result<()> {
        tokio::task::spawn_blocking(move || -> zbus::fdo::Result<()> {
            let server = open_server()?;
            let resp = MoveToFrontRequest::response(&server, id, Some(RingKind::Main))
                .map_err(|e| zbus::fdo::Error::Failed(format!("move_to_front {id}: {e}")))?;
            match resp {
                ringboard_core::protocol::MoveToFrontResponse::Success { id: _ } => Ok(()),
                ringboard_core::protocol::MoveToFrontResponse::Error(e) => {
                    Err(zbus::fdo::Error::Failed(format!("move_to_front {id}: {e:?}")))
                }
            }
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("move_to_front join: {e}")))??;
        Ok(())
    }

    /// Append a new entry to the main ring. Returns the assigned id.
    async fn add(&self, payload: Vec<u8>, mime: &str) -> zbus::fdo::Result<u64> {
        if payload.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("empty payload".into()));
        }
        let mime = mime.to_owned();
        tokio::task::spawn_blocking(move || -> zbus::fdo::Result<u64> {
            let mime_type = ringboard_core::protocol::MimeType::from(&mime)
                .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("invalid mime: {e}")))?;
            let file = payload_memfd(&payload)?;
            let server = open_server()?;
            let resp = AddRequest::response(&server, RingKind::Main, &mime_type, &file)
                .map_err(|e| zbus::fdo::Error::Failed(format!("add: {e}")))?;
            let AddResponse::Success { id } = resp;
            Ok(id)
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("add join: {e}")))?
    }
}

async fn serve() -> zbus::Result<()> {
    let _conn = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Iface)?
        .build()
        .await?;
    info!("DBus interface registered on session bus as {BUS_NAME}");
    // Park forever; zbus dispatches in the background.
    std::future::pending::<()>().await;
    Ok(())
}
