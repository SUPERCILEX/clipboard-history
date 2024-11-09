use std::{fs, marker::PhantomData, path::PathBuf};

use ringboard_core::{IoErr, LeaveBe, acquire_lock_file};
use rustix::fs::{AtFlags, CWD, unlinkat};

use crate::CliError;

#[must_use]
pub struct OwnedServer(PhantomData<()>);

impl OwnedServer {
    #[allow(clippy::unused_self)]
    pub fn shutdown(self) -> Result<(), CliError> {
        unlinkat(CWD, c"server.lock", AtFlags::empty())
            .map_io_err(|| "Failed to delete server lock file.")
            .map_err(CliError::from)
    }
}

pub fn claim_server_ownership() -> Result<OwnedServer, CliError> {
    acquire_lock_file(
        &mut false,
        CWD,
        c".",
        c".server.lock",
        c"server.lock",
        LeaveBe,
    )?
    .map_or(Ok(OwnedServer(PhantomData)), |pid| {
        Err(CliError::ServerAlreadyRunning {
            pid,
            lock_file: fs::canonicalize("server.lock")
                .unwrap_or_else(|_| PathBuf::from("server.lock")),
        })
    })
}
