#![cfg(feature = "dbus")]

use std::thread::{self, JoinHandle};

use log::{info, warn};
use zbus::connection::Builder;

pub const BUS_NAME: &str = "com.github.SUPERCILEX.Ringboard";
pub const OBJECT_PATH: &str = "/com/github/SUPERCILEX/Ringboard";
pub const INTERFACE_NAME: &str = "com.github.SUPERCILEX.Ringboard1";

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

async fn serve() -> zbus::Result<()> {
    let _conn = Builder::session()?
        .name(BUS_NAME)?
        .build()
        .await?;
    info!("DBus interface registered on session bus as {BUS_NAME}");
    // Park forever; zbus dispatches in the background.
    std::future::pending::<()>().await;
    Ok(())
}
