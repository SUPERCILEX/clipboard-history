use std::{
    env,
    path::{PathBuf, MAIN_SEPARATOR},
};

#[must_use]
pub fn data_dir() -> PathBuf {
    let mut dir = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("/tmp/data"));
    dir.reserve("clipboard-history/buckets/(1024, 2048]".len());
    dir.push("clipboard-history");
    dir
}

#[must_use]
pub fn socket_file() -> PathBuf {
    if let Some(s) = env::var_os("RINGBOARD_SOCK") {
        return PathBuf::from(s);
    }

    let mut file = PathBuf::with_capacity("/tmp/.ringboard/username.ch".len());
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
    file.set_extension("ch");
    file
}
