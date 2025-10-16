// SPDX-License-Identifier: Apache-2.0

use std::any::TypeId;

use cosmic::{
    Application,
    cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry},
    iced::Subscription,
};
use ringboard_sdk::ui_actor::SearchKind;
use serde::{Deserialize, Serialize};

use crate::app::{AppMessage, AppModel};

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

struct ConfigSubscription;

pub fn config_sub() -> Subscription<AppMessage> {
    cosmic_config::config_subscription(
        TypeId::of::<ConfigSubscription>(),
        AppModel::APP_ID.into(),
        Config::VERSION,
    )
    .map(|update| AppMessage::ConfigUpdate(update.config))
}
