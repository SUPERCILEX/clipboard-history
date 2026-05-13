#![cfg(feature = "dbus")]

use std::{os::fd::OwnedFd, thread::{self, JoinHandle}};

use log::{info, warn};
use ringboard_core::dirs::{data_dir, socket_file};
use ringboard_sdk::{
    DatabaseReader,
    api::{RemoveRequest, connect_to_server},
};
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
