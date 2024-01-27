use std::path::{PathBuf, MAIN_SEPARATOR};

#[must_use]
pub fn data_dir() -> PathBuf {
    let mut dir = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("/tmp/data"));
    dir.push("clipboard-history");
    dir
}

#[must_use]
pub fn socket_file() -> PathBuf {
    let mut file = PathBuf::from("/tmp");
    file.push(
        dirs::home_dir()
            .as_deref()
            .map(|p| p.to_string_lossy())
            .as_deref()
            .and_then(|p| p.split(MAIN_SEPARATOR).last())
            .unwrap_or("default"),
    );
    file.set_extension("clipboard-history");
    file
}
