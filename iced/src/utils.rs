use ::image as image_crate;
use std::{fs::File, io::BufReader};

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
