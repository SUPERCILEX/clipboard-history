//! C-compatible FFI layer exposing Ringboard client-sdk operations to the
//! GNOME Shell JavaScript extension.
//!
//! The GNOME Shell extension loads this shared library via GModule at runtime
//! and calls the exported `#[unsafe(no_mangle)] pub extern "C"` functions below.
//!
//! Concurrency: GNOME Shell is single-threaded (GLib main loop), so no
//! synchronisation is needed; we store the connection in a static Mutex
//! purely to satisfy Rust's Send requirement across FFI boundaries.

use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::fd::OwnedFd,
    sync::Mutex,
};

use clipboard_history_client_sdk::api::{AddRequest, MoveToFrontRequest, RemoveRequest};
use ringboard_core::{
    create_tmp_file,
    dirs::socket_name,
    protocol::{AddResponse, MimeType, RingKind},
};
use rustix::{
    fs::{Mode, OFlags},
    net::SocketAddrUnix,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum payload size accepted from FFI callers. Prevents a bad length
/// argument from reading unbounded process memory into the Ringboard server.
const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

// ---------------------------------------------------------------------------
// Global connection state
// ---------------------------------------------------------------------------

struct Connection {
    server: OwnedFd,
}

// SAFETY: OwnedFd is Send; GNOME Shell is single-threaded so the Mutex is
// never actually contended, but Rust requires it for static storage.
unsafe impl Send for Connection {}

static CONNECTION: Mutex<Option<Connection>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Init / destroy
// ---------------------------------------------------------------------------

/// Initialise the ringboard connection.
///
/// Returns 0 on success, -1 on failure.
#[unsafe(no_mangle)]
pub extern "C" fn ringboard_init() -> i32 {
    let name = socket_name();
    let addr = match SocketAddrUnix::new(&name) {
        Ok(a) => a,
        Err(_) => return -1,
    };

    match clipboard_history_client_sdk::api::connect_to_server(&addr) {
        Ok(fd) => {
            let mut guard = CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(Connection { server: fd });
            0
        }
        Err(_) => -1,
    }
}

/// Release the ringboard connection.
#[unsafe(no_mangle)]
pub extern "C" fn ringboard_destroy() {
    let mut guard = CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

// ---------------------------------------------------------------------------
// Entry operations
// ---------------------------------------------------------------------------

/// Add a UTF-8 text entry to the Main ring.
///
/// # Parameters
/// * `ptr` — pointer to UTF-8 bytes (not NUL-terminated)
/// * `len` — byte length of the text
///
/// Returns the composite entry ID (>= 0) on success, or -1 on failure.
///
/// # Safety
/// `ptr` must be valid and point to at least `len` bytes of readable memory.
/// `len` must not exceed `MAX_PAYLOAD_BYTES` (16 MiB).
#[unsafe(no_mangle)]
pub extern "C" fn ringboard_add_text(ptr: *const u8, len: usize) -> i64 {
    if ptr.is_null() || len == 0 {
        return -1;
    }
    // Bounds check: reject implausibly large payloads before dereferencing.
    if len > MAX_PAYLOAD_BYTES {
        return -1;
    }

    let text = unsafe { std::slice::from_raw_parts(ptr, len) };

    // Write text to a temporary file so we can pass a file descriptor to AddRequest.
    // Use the system temp directory rather than CWD, which may not be writable
    // in the GNOME Shell extension context.
    let tmp_dir = std::env::temp_dir();
    let dir_fd: OwnedFd = match std::fs::File::open(&tmp_dir) {
        Ok(f) => f.into(),
        Err(_) => return -1,
    };

    let mut tmp_unsupported = false;
    let tmp_fd: OwnedFd = match create_tmp_file(
        &mut tmp_unsupported,
        &dir_fd,
        c".",
        c".ringboard-gnome-add",
        OFlags::RDWR,
        Mode::empty(),
    ) {
        Ok(f) => f,
        Err(_) => return -1,
    };

    // Wrap in File for std::io::Write / Seek convenience.
    let mut tmp_file: File = tmp_fd.into();
    if tmp_file.write_all(text).is_err() {
        return -1;
    }
    if tmp_file.seek(SeekFrom::Start(0)).is_err() {
        return -1;
    }

    let mut mime = MimeType::new();
    let _ = mime.try_push_str("text/plain");

    // Narrow the mutex lock to just the server RPC call, not the file I/O above.
    let guard = CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
    let conn = match guard.as_ref() {
        Some(c) => c,
        None => return -1,
    };

    match AddRequest::response(&conn.server, RingKind::Main, &mime, &tmp_file) {
        Ok(AddResponse::Success { id }) => i64::try_from(id).unwrap_or(-1),
        Err(_) => -1,
    }
}

/// Move an existing entry to the front of its ring (mark as most-recently-used).
///
/// Returns 0 on success, -1 on failure.
#[unsafe(no_mangle)]
pub extern "C" fn ringboard_move_to_front(id: u64) -> i32 {
    let guard = CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
    let conn = match guard.as_ref() {
        Some(c) => c,
        None => return -1,
    };

    match MoveToFrontRequest::response(&conn.server, id, None) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Remove an entry from the ring.
///
/// Returns 0 on success, -1 on failure.
#[unsafe(no_mangle)]
pub extern "C" fn ringboard_remove(id: u64) -> i32 {
    let guard = CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
    let conn = match guard.as_ref() {
        Some(c) => c,
        None => return -1,
    };

    match RemoveRequest::response(&conn.server, id) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Populate `buf` with up to `max_entries` null-terminated UTF-8 C strings
/// representing the most recent history entries (most-recent first).
///
/// Each slot in `buf` is a `*mut u8` pointer to a heap-allocated C string
/// (caller must free with `ringboard_free_entry`).
///
/// Returns the number of entries written (may be 0), or -1 on error.
///
/// Note: The browse UI reads from the in-memory `DS.LinkedList` maintained by
/// `extension.js` rather than via this function; this entry point exists for
/// completeness and future use.
#[unsafe(no_mangle)]
pub extern "C" fn ringboard_get_entries(buf: *mut *mut u8, max_entries: usize) -> i32 {
    if buf.is_null() || max_entries == 0 {
        return -1;
    }
    // The JS layer owns the in-memory cache; server-side listing is out of
    // scope for phase 1.
    0
}

/// Free a C string previously returned by `ringboard_get_entries`.
#[unsafe(no_mangle)]
pub extern "C" fn ringboard_free_entry(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // Reconstruct the CString to drop it and reclaim memory.
    unsafe { drop(std::ffi::CString::from_raw(ptr.cast::<std::ffi::c_char>())) };
}
