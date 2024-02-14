use std::{
    fmt::Write as FmtWrite,
    fs::File,
    io::{ErrorKind::AlreadyExists, Write},
    num::NonZeroU32,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process,
};

use clipboard_history_core::{read_server_pid, IoErr};
use rustix::{
    fs::{linkat, openat, unlinkat, AtFlags, Mode, OFlags, CWD},
    io::Errno,
    process::{test_kill_process, Pid},
};

use crate::CliError;

#[must_use]
pub struct OwnedServer(PathBuf);

impl OwnedServer {
    pub fn shutdown(self) -> Result<(), CliError> {
        unlinkat(CWD, &self.0, AtFlags::empty())
            .map_io_err(|| format!("Failed to delete server lock file: {:?}", self.0))
            .map_err(CliError::from)
    }
}

pub fn claim_server_ownership(lock_file_path: &Path) -> Result<Option<OwnedServer>, CliError> {
    let mut lock_file = File::from(
        openat(
            CWD,
            lock_file_path.parent().unwrap(),
            OFlags::WRONLY | OFlags::TMPFILE,
            Mode::RUSR,
        )
        .map_io_err(|| format!("Failed to create server lock temp file: {lock_file_path:?}"))?,
    );

    {
        let mut buf = itoa::Buffer::new();
        lock_file
            .write_all(buf.format(process::id()).as_bytes())
            .map_io_err(|| format!("Failed to write to server lock file: {lock_file_path:?}"))?;
    }

    #[allow(clippy::blocks_in_conditions)]
    match {
        let mut s = String::from("/proc/self/fd/");
        write!(s, "{}", lock_file.as_raw_fd()).unwrap();
        linkat(CWD, &s, CWD, lock_file_path, AtFlags::SYMLINK_FOLLOW)
    } {
        Err(e) if e.kind() == AlreadyExists => {
            let pid = read_server_pid(lock_file_path)?;
            let Some(pid) = NonZeroU32::new(pid) else {
                return Ok(None);
            };

            match test_kill_process(unsafe {
                Pid::from_raw_unchecked(i32::try_from(pid.get()).unwrap())
            }) {
                Err(e) if e == Errno::SRCH => {
                    return Ok(None);
                }
                r => r.map_io_err(|| format!("Failed to check server status (PID {pid})."))?,
            }

            return Err(CliError::ServerAlreadyRunning {
                pid,
                lock_file: lock_file_path.to_path_buf(),
            });
        }
        r => r.map_io_err(|| format!("Failed to acquire server lock: {lock_file_path:?}"))?,
    };

    Ok(Some(OwnedServer(lock_file_path.to_path_buf())))
}
