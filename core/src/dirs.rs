use std::{env, ffi::OsString, path::PathBuf};

#[must_use]
pub fn data_dir() -> PathBuf {
    let mut dir = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("/tmp/data"));
    dir.reserve("/clipboard-history/buckets/(1024, 2048]".len());
    dir.push("clipboard-history");
    dir
}

#[must_use]
pub fn socket_name() -> OsString {
    if let Some(s) = env::var_os("RINGBOARD_SOCK") {
        return s;
    }

    let mut name = OsString::with_capacity(
        "ringboard-server:/home/username/.local/share/clipboard-history".len(),
    );
    name.push("ringboard-server:");
    name.push(data_dir());
    name
}

#[must_use]
pub fn paste_socket_name() -> OsString {
    if let Some(s) = env::var_os("PASTE_SOCK") {
        return s;
    }

    let mut name = OsString::with_capacity(
        "ringboard-paste:/home/username/.local/share/clipboard-history".len(),
    );
    name.push("ringboard-paste:");
    name.push(data_dir());
    name
}

#[must_use]
pub fn config_file_dir() -> PathBuf {
    let mut dir = dirs::config_local_dir().unwrap_or_else(|| PathBuf::from("/tmp/config"));
    dir.reserve("/ringboard/wayland.toml".len());
    dir.push("ringboard");
    dir
}
