use std::{env, ffi::OsStr};

use cosmic::{
    Application,
    app::Settings,
    cosmic_config::{self, CosmicConfigEntry},
};

use crate::{
    app::{Flags, Model},
    config::Config,
};

mod app;
mod client;
mod config;
mod i18n;
mod ipc;
mod views;

#[derive(Default, Copy, Clone, Debug)]
pub enum AppMode {
    #[default]
    Normal,
    Applet,
}

#[derive(Default)]
struct Cli {
    mode: AppMode,
    toggle: bool,
}

fn cli() -> Cli {
    let mut args = env::args_os().skip(1);
    let arg = args.next();
    if arg.as_deref() == Some(OsStr::new("toggle")) {
        return Cli {
            mode: AppMode::Normal,
            toggle: true,
        };
    }
    if arg.as_deref() == Some(OsStr::new("applet")) {
        let arg = args.next();
        Cli {
            mode: AppMode::Applet,
            toggle: arg.as_deref() == Some(OsStr::new("toggle")),
        }
    } else {
        Cli::default()
    }
}

fn init_flags<const APPLET: bool>() -> Flags {
    let config_manager = cosmic_config::Config::new(Model::<APPLET>::APP_ID, Config::VERSION)
        .inspect_err(|err| eprintln!("Failed to initialize config manager: {err}"))
        .ok();
    let config = config_manager
        .as_ref()
        .map(|m| {
            Config::get_entry(m).unwrap_or_else(|(errs, c)| {
                eprintln!("Failed to load config: {errs:?}");
                c
            })
        })
        .unwrap_or_default();

    Flags {
        config_manager,
        config,
    }
}

#[tokio::main]
async fn main() {
    let Cli { mode, toggle } = cli();
    if toggle {
        todo!();
    }

    unsafe {
        i18n::init();
    }

    match mode {
        AppMode::Normal => {
            cosmic::app::run::<Model<false>>(Settings::default(), init_flags::<false>())
        }
        AppMode::Applet => cosmic::applet::run::<Model<true>>(init_flags::<true>()),
    }
    .unwrap();
}
