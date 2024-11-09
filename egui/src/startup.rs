use std::{
    ffi::CString,
    fmt::Debug,
    mem::MaybeUninit,
    os::{fd::AsFd, unix::ffi::OsStringExt},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use ringboard_sdk::core::{
    Error as CoreError, IoErr, SendKillAndTakeover, acquire_lock_file, dirs::push_sockets_prefix,
};
use rustix::{
    fs::{CWD, inotify, inotify::ReadFlags},
    path::Arg,
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
    let mut cache = Default::default();
    let path = sleep_file_name();
    let inotify =
        inotify::init(inotify::CreateFlags::empty()).map_io_err(|| "Failed to create inotify.")?;
    loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }

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
