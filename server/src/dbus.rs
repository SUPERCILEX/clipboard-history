// DBus front-end for ringboard-server.
//
// The interface is hosted by a worker thread that runs a tokio
// current-thread runtime and a zbus connection. Mutations are issued
// through the server's Unix socket via the client-sdk; read queries open
// the ring files directly. The io_uring reactor in reactor.rs is not
// touched.

#![cfg(feature = "dbus")]

use std::thread::{self, JoinHandle};

/// Spawn the DBus worker. The returned `JoinHandle` is intentionally
/// dropped on shutdown — the thread is a daemon, and the process exiting
/// tears down the zbus connection cleanly.
#[allow(clippy::missing_errors_doc)]
pub fn spawn() -> JoinHandle<()> {
    thread::Builder::new()
        .name("ringboard-dbus".into())
        .spawn(|| {
            // Real worker arrives in Task 2.
        })
        .expect("failed to spawn ringboard-dbus thread")
}
