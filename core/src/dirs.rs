use std::{
    env,
    path::{MAIN_SEPARATOR, PathBuf},
};

#[must_use]
pub fn data_dir() -> PathBuf {
    let mut dir = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("/tmp/data"));
    dir.reserve("/clipboard-history/buckets/(1024, 2048]".len());
    dir.push("clipboard-history");
    dir
}

#[must_use]
pub fn socket_file() -> PathBuf {
    if let Some(s) = env::var_os("RINGBOARD_SOCK") {
        return PathBuf::from(s);
    }

    let mut file = PathBuf::with_capacity("/tmp/.ringboard/username.ch".len());
    push_sockets_prefix(&mut file);
    file.set_extension("ch");
    file
}

#[must_use]
pub fn paste_socket_file() -> PathBuf {
    if let Some(s) = env::var_os("PASTE_SOCK") {
        return PathBuf::from(s);
    }

    let mut file = PathBuf::with_capacity("/tmp/.ringboard/username.paste".len());
    push_sockets_prefix(&mut file);
    file.set_extension("paste");
    file
}

pub fn push_sockets_prefix(file: &mut PathBuf) {
    #[allow(clippy::path_buf_push_overwrite)]
    file.push("/tmp/.ringboard");
    file.push(
        dirs::home_dir()
            .as_deref()
            .map(|p| p.to_string_lossy())
            .as_deref()
            .and_then(|p| p.rsplit(MAIN_SEPARATOR).next())
            .unwrap_or("default"),
    );
}

#[must_use]
pub fn config_file_dir() -> PathBuf {
    let mut dir = dirs::config_local_dir().unwrap_or_else(|| PathBuf::from("/tmp/config"));
    dir.reserve("/ringboard/x11.toml".len());
    dir.push("ringboard");
    dir
}
