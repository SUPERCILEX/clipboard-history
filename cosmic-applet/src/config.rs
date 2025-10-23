// SPDX-License-Identifier: Apache-2.0

use std::any::TypeId;

use cosmic::{
    Application,
    cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry},
    iced::Subscription,
    iced_winit::commands::subsurface::Anchor,
};
use ringboard_sdk::ui_actor::SearchKind;
use serde::{Deserialize, Serialize};

use crate::app::{AppMessage, AppModel};

#[derive(Debug, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    pub filter_mode: FilterMode,
    pub horizontal_position: Position,
    pub vertical_position: Position,
}

impl Config {
    pub fn anchor(&self) -> Anchor {
        let horizontal = match self.horizontal_position {
            Position::Start => Anchor::LEFT,
            Position::Center => Anchor::empty(),
            Position::End => Anchor::RIGHT,
        };

        let vertical = match self.vertical_position {
            Position::Start => Anchor::TOP,
            Position::Center => Anchor::empty(),
            Position::End => Anchor::BOTTOM,
        };

        horizontal | vertical
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            filter_mode: FilterMode::default(),
            horizontal_position: Position::End,
            vertical_position: Position::Start,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
pub enum FilterMode {
    #[default]
    Plain,
    Regex,
    Mime,
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
pub enum Position {
    #[default]
    Center,
    Start,
    End,
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
