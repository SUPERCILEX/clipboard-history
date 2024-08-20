use std::path::PathBuf;

use ringboard_core::dirs::config_file_dir;
use serde::{Deserialize, Serialize};

#[must_use]
pub fn x11_config_file() -> PathBuf {
    let mut file = config_file_dir();
    file.push("x11.toml");
    file
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "version")]
pub enum X11Config {
    V1(X11V1Config),
}

impl Default for X11Config {
    fn default() -> Self {
        Self::V1(X11V1Config::default())
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename = "v1")]
pub struct X11V1Config {
    #[serde(default = "x11_auto_paste_")]
    pub auto_paste: bool,
}

impl Default for X11V1Config {
    fn default() -> Self {
        Self {
            auto_paste: x11_auto_paste_(),
        }
    }
}

const fn x11_auto_paste_() -> bool {
    true
}
