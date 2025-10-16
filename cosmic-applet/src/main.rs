// SPDX-License-Identifier: Apache-2.0

use cosmic::{
    Application,
    cosmic_config::{self, CosmicConfigEntry},
};
use tracing::{debug, info};

use crate::{app::Flags, config::Config};

mod app;
mod config;
mod i18n;
mod util;
mod views;

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt().init();
    // Get the system's preferred languages.
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();

    debug!("Loaded languages: {:?}", requested_languages);
    // Enable localizations to be applied.
    i18n::init(&requested_languages);

    debug!(
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
    };

    info!("Starting app with config: {:?}", flags.config);
    // Starts the application's event loop with `()` as the application's flags.
    cosmic::applet::run::<app::AppModel>(flags)
}
