use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, CosmicConfigEntry, PartialEq, Eq)]
#[version = 1]
pub struct GeneralConfig {
    pub items_max: u32,
    pub show_favourites: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            items_max: 50,
            show_favourites: true,
        }
    }
}