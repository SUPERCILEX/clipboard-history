use std::{
    ffi::{CString, OsStr},
    fs,
    mem::MaybeUninit,
    os::{
        fd::AsFd,
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{MAIN_SEPARATOR, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
};

use ringboard_sdk::core::{
    Error as CoreError, IoErr, LeaveBe, OwnedLockFile, SendKillAndTakeover, acquire_lock_file,
};
use rustix::fs::{AtFlags, CWD, inotify, inotify::ReadFlags, unlinkat};

pub fn sleep_file_name() -> CString {
    CString::new(sleep_file_name_().into_os_string().into_vec()).unwrap()
}

fn sleep_file_name_() -> PathBuf {
    let mut path = PathBuf::with_capacity("/tmp/.ringboard/username/egui/sleep.lock".len());
    #[allow(clippy::path_buf_push_overwrite)]
    path.push("/tmp/.ringboard");
    path.push(
        dirs::home_dir()
            .as_deref()
            .map(|p| p.to_string_lossy())
            .as_deref()
            .and_then(|p| p.rsplit(MAIN_SEPARATOR).next())
            .unwrap_or("default"),
    );
    path.push("egui/sleep.lock");
    path
}

pub struct Token(PathBuf, OwnedLockFile);

pub fn maybe_open_existing_instance_and_exit() -> Result<Token, CoreError> {
    let path = sleep_file_name_();
    fs::create_dir_all(path.parent().unwrap())
        .map_io_err(|| format!("Failed to create sleep lock file directory: {path:?}"))?;
    if let Ok(lock) = acquire_lock_file(&path, LeaveBe)? {
        Ok(Token(path, lock))
    } else {
        unlinkat(CWD, &path, AtFlags::empty())
            .map_io_err(|| format!("Failed to remove sleep file: {path:?}"))?;
        ExitCode::SUCCESS.exit_process()
    }
}

pub fn maintain_single_instance(
    stop: &AtomicBool,
    token: Option<Token>,
    mut open: impl FnMut(),
) -> Result<(), CoreError> {
    let path;
    let mut _lock;
    if let Some(Token(path_, lock_)) = token {
        path = path_;
        _lock = lock_;
    } else {
        path = sleep_file_name_();
    }
    let path_dir = path.parent().unwrap();
    let path_name = path.file_name().unwrap();
    let inotify =
        inotify::init(inotify::CreateFlags::empty()).map_io_err(|| "Failed to create inotify.")?;

    let mut watch = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }

        fs::create_dir_all(path_dir)
            .map_io_err(|| format!("Failed to create sleep lock file directory: {path_dir:?}"))?;
        if watch.is_none() {
            let id = inotify::add_watch(
                &inotify,
                path_dir,
                inotify::WatchFlags::MOVE_SELF
                    | inotify::WatchFlags::DELETE_SELF
                    | inotify::WatchFlags::DELETE
                    | inotify::WatchFlags::MOVED_FROM
                    | inotify::WatchFlags::ONLYDIR
                    | inotify::WatchFlags::MASK_CREATE,
            )
            .map_io_err(|| "Failed to register inotify watch.")?;
            watch = Some(id);
        }
        _lock = acquire_lock_file(&path, SendKillAndTakeover)?;
        wait_for_sleep_cancel(&inotify, &mut watch, path_name, &mut open)?;
    }
}

fn wait_for_sleep_cancel(
    inotify: impl AsFd,
    watch_id: &mut Option<i32>,
    path_name: &OsStr,
    mut open: impl FnMut(),
) -> Result<(), CoreError> {
    let mut watch_deleted = false;
    let mut file_changed = false;
    {
        let mut buf = [MaybeUninit::uninit(); 128];
        let mut buf = inotify::Reader::new(&inotify, &mut buf);
        loop {
            let e = buf.next().map_io_err(|| "Failed to read inotify events")?;
            let mut dir_moved = false;
            if Some(e.wd()) == *watch_id {
                dir_moved = e.events().contains(ReadFlags::MOVE_SELF);
                watch_deleted |= e.events().contains(ReadFlags::IGNORED);
                file_changed |= e
                    .file_name()
                    .is_some_and(|f| f.to_bytes() == path_name.as_bytes());

                if dir_moved && let Some(watch_id) = watch_id.take() {
                    inotify::remove_watch(&inotify, watch_id)
                        .map_io_err(|| "Failed to remove inotify watch.")?;
                }
                if watch_deleted {
                    *watch_id = None;
                }
            }

            if buf.is_buffer_empty() && (dir_moved || watch_deleted || file_changed) {
                break;
            }
        }
    }
    open();
    Ok(())
}
