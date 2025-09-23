use std::{
    ffi::CString,
    fmt::Debug,
    fs,
    fs::File,
    mem::MaybeUninit,
    os::{fd::AsFd, unix::ffi::OsStringExt},
    path::PathBuf,
    process::exit,
    sync::atomic::{AtomicBool, Ordering},
};

use ringboard_sdk::core::{
    Error as CoreError, IoErr, LockFilePid, SendKillAndTakeover, acquire_lock_file,
    dirs::push_sockets_prefix, read_lock_file_pid,
};
use rustix::{
    fs::{AtFlags, CWD, Mode, OFlags, inotify, inotify::ReadFlags, openat, unlinkat},
    io::Errno,
    path::Arg,
    process::test_kill_process,
};

pub fn sleep_file_name() -> CString {
    let mut path = PathBuf::with_capacity("/tmp/.ringboard/username.egui-sleep".len());
    push_sockets_prefix(&mut path);
    path.set_extension("egui-sleep");
    CString::new(path.into_os_string().into_vec()).unwrap()
}

pub fn maybe_open_existing_instance_and_exit() -> Result<(), CoreError> {
    let path = sleep_file_name();
    let sleep_file = match openat(CWD, &path, OFlags::RDONLY, Mode::empty()) {
        Err(Errno::NOENT) => return Ok(()),
        r => File::from(r.map_io_err(|| format!("Failed to open sleep file: {path:?}"))?),
    };
    let existing_instance = match read_lock_file_pid(&path, &sleep_file)? {
        LockFilePid::Valid(pid) => pid,
        LockFilePid::Deleted | LockFilePid::UserReset => return Ok(()),
    };
    match test_kill_process(existing_instance) {
        Err(Errno::SRCH) => return Ok(()),
        Err(Errno::PERM) => (),
        r => {
            r.map_io_err(|| format!("Failed to check PID for existence: {existing_instance:?}"))?;
        }
    }

    unlinkat(CWD, &path, AtFlags::empty())
        .map_io_err(|| format!("Failed to remove sleep file: {path:?}"))?;
    exit(0)
}

pub fn maintain_single_instance(
    stop: &AtomicBool,
    mut open: impl FnMut(),
) -> Result<(), CoreError> {
    let mut cache = Default::default();
    let path = sleep_file_name();
    let inotify =
        inotify::init(inotify::CreateFlags::empty()).map_io_err(|| "Failed to create inotify.")?;
    loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }

        fs::create_dir_all("/tmp/.ringboard")
            .map_io_err(|| "Failed to create tmp ringboard directory.")?;
        kill_old_instances_if_any(&mut cache, &path)?;
        let id = inotify::add_watch(
            &inotify,
            &path,
            inotify::WatchFlags::MOVE_SELF | inotify::WatchFlags::DELETE_SELF,
        )
        .map_io_err(|| "Failed to register inotify watch.")?;
        wait_for_sleep_cancel(&inotify, id, &mut open)?;
    }
}

fn kill_old_instances_if_any(
    tmp_file_unsupported: &mut bool,
    path: impl Arg + Copy + Debug,
) -> Result<(), CoreError> {
    acquire_lock_file(
        tmp_file_unsupported,
        CWD,
        c"/tmp",
        c"/tmp/.ringboard-egui-lock-scratchpad",
        path,
        SendKillAndTakeover,
    )?;
    Ok(())
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
