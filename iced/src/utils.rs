use ::image as image_crate;
use std::{
    fs::{self, File},
    io::BufReader,
    num::NonZeroU32,
};

use ringboard_sdk::config::server as server_config;

/// Decode an image file asynchronously off the main thread.
pub async fn decode_image_async(
    id: u64,
    file: File,
) -> (u64, Result<image_crate::DynamicImage, String>) {
    let result = tokio::task::spawn_blocking(move || {
        image_crate::ImageReader::new(BufReader::new(file))
            .with_guessed_format()
            .map_err(|e| format!("Failed to guess format: {e}"))
            .and_then(|r| r.decode().map_err(|e| format!("Failed to decode: {e}")))
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panicked: {e}")));
    (id, result)
}

fn server_config_path() -> std::path::PathBuf {
    let mut path = ringboard_sdk::core::dirs::data_dir();
    path.push(server_config::file_name());
    path
}

/// Load the server's `max_entries` limits (main, favorites) from its config
/// file, off the main thread.
pub async fn load_server_config_async() -> Result<(u32, u32), String> {
    tokio::task::spawn_blocking(|| {
        let config = server_config::load(server_config_path()).map_err(|e| e.to_string())?;
        Ok((
            config.max_entries.main.get(),
            config.max_entries.favorites.get(),
        ))
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panicked: {e}")))
}

/// Persist the server's `max_entries` limits to its config file, off the
/// main thread. The server must be restarted for changes to take effect.
pub async fn save_server_config_async(max_main: u32, max_favorites: u32) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let max_main = NonZeroU32::new(max_main).ok_or("Max main entries must be at least 1")?;
        let max_favorites =
            NonZeroU32::new(max_favorites).ok_or("Max favorite entries must be at least 1")?;

        let path = server_config_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }

        let config = server_config::Config {
            max_entries: server_config::MaxEntries {
                main: max_main,
                favorites: max_favorites,
            },
        };
        let contents = toml::to_string_pretty(&server_config::Stable::from(config))
            .map_err(|e| e.to_string())?;
        fs::write(&path, contents).map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panicked: {e}")))
}
