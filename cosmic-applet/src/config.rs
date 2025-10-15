// SPDX-License-Identifier: Apache-2.0

use std::fmt::Display;

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};
use ringboard_sdk::ui_actor::SearchKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    pub filter_mode: FilterMode,
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
pub enum FilterMode {
    #[default]
    Plain,
    Regex,
    Mime,
}

impl From<FilterMode> for SearchKind {
    fn from(value: FilterMode) -> Self {
        match value {
            FilterMode::Plain => SearchKind::Plain,
            FilterMode::Regex => SearchKind::Regex,
            FilterMode::Mime => SearchKind::Mime,
        }
    }
}

impl Display for FilterMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FilterMode::Plain => "Plain",
            FilterMode::Regex => "Regex",
            FilterMode::Mime => "Mime",
        };
        write!(f, "{s}")
    }
}
