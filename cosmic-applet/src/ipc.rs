use std::{any::TypeId, path::PathBuf, time::SystemTime};

use cosmic::iced::{Subscription, stream::channel};
use futures_util::SinkExt;
use notify::Watcher;
use tokio::sync::mpsc;
use tracing::info;

use crate::app::AppMessage;

struct WackupSubscription;

fn signal_file_path() -> PathBuf {
    let path = PathBuf::from("/tmp/ringboard-cosmic-applet-wackup-signal");
    if !path.exists() {
        let _ = std::fs::File::create(&path);
    }
    path
}

pub fn toggle() {
    info!("Sending toggle signal to running instance");
    let path = signal_file_path();
    let time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
        .to_string();
    std::fs::write(&path, time).expect("Failed to write wackup signal file");
    info!("Wackup signal sent");
}

pub fn wackup_sub() -> Subscription<AppMessage> {
    Subscription::run_with_id(
        TypeId::of::<WackupSubscription>(),
        channel(1, move |mut output| async move {
            let path = signal_file_path();

            let (tx, mut rx) = mpsc::channel(1);

            info!("Starting wackup file watcher on {:?}", path);
            let mut watcher =
                notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                    if res.is_ok() {
                        let _ = tx.blocking_send(());
                    }
                })
                .unwrap();

            let _ = watcher.watch(&path, notify::RecursiveMode::NonRecursive);

            info!("Wackup file watcher started");
            loop {
                rx.recv().await;
                info!("Wackup signal received, toggling popup");
                let _ = output.send(AppMessage::TogglePopup).await;
            }
        }),
    )
}
