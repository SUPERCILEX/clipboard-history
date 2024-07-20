use std::{
    fs::File,
    io::{ErrorKind::AlreadyExists, Write},
    os::fd::OwnedFd,
    path::Path,
    process,
};

use ringboard_core::{read_server_pid, IoErr};
use rustix::{
    fs::{openat, unlinkat, AtFlags, Mode, OFlags, CWD},
    io::Errno,
    process::test_kill_process,
};

use crate::{utils::link_tmp_file, CliError};

#[must_use]
pub struct OwnedServer(OwnedFd);

impl OwnedServer {
    pub fn shutdown(self) -> Result<(), CliError> {
        unlinkat(self.0, c"server.lock", AtFlags::empty())
            .map_io_err(|| "Failed to delete server lock file.")
            .map_err(CliError::from)
    }
}

pub fn claim_server_ownership(data_dir: &Path) -> Result<Option<OwnedServer>, CliError> {
    let dir = openat(
        CWD,
        data_dir,
        OFlags::DIRECTORY | OFlags::PATH,
        Mode::empty(),
    )
    .map_io_err(|| format!("Failed to open directory: {data_dir:?}"))?;
    let mut lock_file = File::from(
        openat(&dir, c".", OFlags::WRONLY | OFlags::TMPFILE, Mode::RUSR)
            .map_io_err(|| "Failed to create server lock temp file.")?,
    );

    write!(lock_file, "{}", process::id()).map_io_err(|| "Failed to write to server lock file.")?;

    match link_tmp_file(lock_file, &dir, c"server.lock") {
        Err(e) if e.kind() == AlreadyExists => {
            let pid = read_server_pid(&dir, c"server.lock")?;
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
                lock_file: data_dir.join("server.lock"),
            });
        }
        r => r.map_io_err(|| {
            format!(
                "Failed to acquire server lock: {:?}",
                data_dir.join("server.lock")
            )
        })?,
    };

    Ok(Some(OwnedServer(dir)))
}
