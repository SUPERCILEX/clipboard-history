use std::{
    fs,
    fs::File,
    io::{ErrorKind::AlreadyExists, Write},
    marker::PhantomData,
    path::PathBuf,
    process,
};

use ringboard_core::{link_tmp_file, read_server_pid, IoErr};
use rustix::{
    fs::{openat, unlinkat, AtFlags, Mode, OFlags, CWD},
    io::Errno,
    process::test_kill_process,
};

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

pub fn claim_server_ownership() -> Result<Option<OwnedServer>, CliError> {
    let mut lock_file = File::from(
        openat(CWD, c".", OFlags::WRONLY | OFlags::TMPFILE, Mode::RUSR)
            .map_io_err(|| "Failed to create server lock temp file.")?,
    );

    write!(lock_file, "{}", process::id()).map_io_err(|| "Failed to write to server lock file.")?;

    match link_tmp_file(lock_file, CWD, c"server.lock") {
        Err(e) if e.kind() == AlreadyExists => {
            let pid = read_server_pid(CWD, c"server.lock")?;
            let Some(pid) = pid else {
                return Ok(None);
            };

            match test_kill_process(pid) {
                Err(Errno::SRCH) => {
                    return Ok(None);
                }
                r => r.map_io_err(|| format!("Failed to check server status: {pid:?}."))?,
            }

            return Err(CliError::ServerAlreadyRunning {
                pid,
                lock_file: fs::canonicalize("server.lock")
                    .unwrap_or_else(|_| PathBuf::from("server.lock")),
            });
        }
        r => r.map_io_err(|| {
            format!(
                "Failed to acquire server lock: {:?}",
                fs::canonicalize("server.lock").unwrap_or_else(|_| PathBuf::from("server.lock"))
            )
        })?,
    };

    Ok(Some(OwnedServer(PhantomData)))
}
