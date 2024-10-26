use std::{
    ffi::CString,
    fmt::Debug,
    fs::File,
    io::{ErrorKind::AlreadyExists, Write},
    mem::MaybeUninit,
    os::{fd::AsFd, unix::ffi::OsStringExt},
    path::PathBuf,
    process,
};

use ringboard_sdk::core::{
    Error as CoreError, IoErr, dirs::push_sockets_prefix, link_tmp_file, read_lock_file_pid,
};
use rustix::{
    fs::{
        CWD, Mode, OFlags, inotify,
        inotify::{inotify_add_watch, inotify_init},
        openat, unlink,
    },
    io::{Errno, read_uninit},
    path::Arg,
    process::{Signal, getpid, kill_process},
};

pub fn maintain_single_instance(mut open: impl FnMut()) -> Result<(), CoreError> {
    let mut path = PathBuf::with_capacity("/tmp/.ringboard/username.egui-sleep".len());
    push_sockets_prefix(&mut path);
    path.set_extension("egui-sleep");
    let path = CString::new(path.into_os_string().into_vec()).unwrap();

    let inotify =
        inotify_init(inotify::CreateFlags::empty()).map_io_err(|| "Failed to create inotify.")?;
    loop {
        kill_old_instances_if_any(&path)?;
        inotify_add_watch(inotify.as_fd(), &path, inotify::WatchFlags::MOVE_SELF)
            .map_io_err(|| "Failed to register inotify watch.")?;
        wait_for_sleep_cancel(&inotify, &mut open)?;
    }
}

fn kill_old_instances_if_any(path: impl Arg + Copy + Debug) -> Result<(), CoreError> {
    let mut lock_file = File::from(
        openat(CWD, c"/tmp", OFlags::WRONLY | OFlags::TMPFILE, Mode::RUSR)
            .map_io_err(|| "Failed to create egui sleep temp file.")?,
    );

    write!(lock_file, "{}", process::id()).map_io_err(|| "Failed to write to egui sleep file.")?;

    loop {
        match link_tmp_file(&lock_file, CWD, path) {
            Err(e) if e.kind() == AlreadyExists => 'link: {
                let pid = read_lock_file_pid(CWD, path)?;
                let Some(pid) = pid else {
                    break 'link;
                };

                if pid == getpid() {
                    return Ok(());
                }

                match kill_process(pid, Signal::Term) {
                    Err(Errno::SRCH) => {
                        // Already dead
                    }
                    r => {
                        r.map_io_err(|| format!("Failed to kill other egui instance: {pid:?}."))?;
                    }
                }
            }
            r => {
                r.map_io_err(|| format!("Failed to acquire egui sleep lock: {path:?}",))?;
                return Ok(());
            }
        };

        unlink(path)
            .map_io_err(|| format!("Failed to remove previous egui sleep file: {path:?}"))?;
    }
}

fn wait_for_sleep_cancel(inotify: impl AsFd, mut open: impl FnMut()) -> Result<(), CoreError> {
    // TODO https://github.com/bytecodealliance/rustix/issues/538#issuecomment-2076539826
    let mut buf = [MaybeUninit::uninit(); 32];
    read_uninit(inotify, &mut buf).map_io_err(|| "Failed to read inotify event.")?;
    open();
    Ok(())
}
