// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use cosmic::{
    Application,
    cosmic_config::{self, CosmicConfigEntry},
};
use tokio::sync::Notify;
use tracing::info;

use crate::{app::Flags, config::Config, logging::init_logging};

mod app;
mod client;
mod config;
mod dbus;
mod i18n;
mod logging;
mod util;
mod views;

#[tokio::main]
async fn main() -> cosmic::iced::Result {
    init_logging();

    let running = dbus::client().await.expect("Failed to contact D-Bus");
    if running {
        info!("Another instance is already running, exiting.");
        return Ok(());
    }

    let notify = Arc::new(Notify::new());
    dbus::server(notify.clone())
        .await
        .expect("Failed to start D-Bus server");

    // Get the system's preferred languages.
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();

    info!("Loaded languages: {:?}", requested_languages);
    // Enable localizations to be applied.
    i18n::init(&requested_languages);

    info!(
        "Loading config for app id {} version {}",
        app::AppModel::APP_ID,
        Config::VERSION
    );
    let (config_handler, config) =
        match cosmic_config::Config::new(app::AppModel::APP_ID, Config::VERSION) {
            Ok(config_handler) => {
                let config = match Config::get_entry(&config_handler) {
                    Ok(ok) => ok,
                    Err((errs, config)) => {
                        println!("errors loading config: {:?}", errs);
                        config
                    }
                };
                (config_handler, config)
            }
            Err(err) => {
                panic!("failed to create config handler: {}", err);
            }
        };

    let flags = Flags {
        config_handler,
        config,
        notify,
    };

    info!("Starting app with config: {:?}", flags.config);
    // Starts the application's event loop with `()` as the application's flags.
    cosmic::applet::run::<app::AppModel>(flags)
}
