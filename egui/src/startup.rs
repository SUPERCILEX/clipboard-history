use std::{
    ffi::CString,
    fmt::Debug,
    fs::File,
    io::{ErrorKind::AlreadyExists, Write},
    mem::MaybeUninit,
    os::{fd::AsFd, unix::ffi::OsStringExt},
    path::PathBuf,
    process,
    sync::atomic::{AtomicBool, Ordering},
};

use ringboard_sdk::core::{
    Error as CoreError, IoErr, dirs::push_sockets_prefix, link_tmp_file, read_lock_file_pid,
};
use rustix::{
    fs::{CWD, Mode, OFlags, inotify, inotify::ReadFlags, openat, unlink},
    io::Errno,
    path::Arg,
    process::{Signal, getpid, kill_process},
};

pub fn sleep_file_name() -> CString {
    let mut path = PathBuf::with_capacity("/tmp/.ringboard/username.egui-sleep".len());
    push_sockets_prefix(&mut path);
    path.set_extension("egui-sleep");
    CString::new(path.into_os_string().into_vec()).unwrap()
}

pub fn maintain_single_instance(
    stop: &AtomicBool,
    mut open: impl FnMut(),
) -> Result<(), CoreError> {
    let path = sleep_file_name();
    let inotify =
        inotify::init(inotify::CreateFlags::empty()).map_io_err(|| "Failed to create inotify.")?;
    loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }

        kill_old_instances_if_any(&path)?;
        let id = inotify::add_watch(
            &inotify,
            &path,
            inotify::WatchFlags::MOVE_SELF | inotify::WatchFlags::DELETE_SELF,
        )
        .map_io_err(|| "Failed to register inotify watch.")?;
        wait_for_sleep_cancel(&inotify, id, &mut open)?;
    }
}

fn kill_old_instances_if_any(path: impl Arg + Copy + Debug) -> Result<(), CoreError> {
    let mut lock_file = File::from(
        openat(CWD, c"/tmp", OFlags::WRONLY | OFlags::TMPFILE, Mode::RUSR)
            .map_io_err(|| "Failed to create egui sleep temp file.")?,
    );

    writeln!(lock_file, "{}", process::id())
        .map_io_err(|| "Failed to write to egui sleep file.")?;

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

fn wait_for_sleep_cancel(
    inotify: impl AsFd,
    watch_id: i32,
    mut open: impl FnMut(),
) -> Result<(), CoreError> {
    {
        let mut watch_deleted = false;

        let mut buf = [MaybeUninit::uninit(); 96];
        let mut buf = inotify::Reader::new(&inotify, &mut buf);
        loop {
            let e = buf.next().map_io_err(|| "Failed to read inotify events")?;
            watch_deleted |= e.events().contains(ReadFlags::IGNORED);
            if buf.is_buffer_empty() {
                break;
            }
        }

        if !watch_deleted {
            inotify::remove_watch(&inotify, watch_id)
                .map_io_err(|| "Failed to remove inotify watch.")?;
        }
    }
    open();
    Ok(())
}
