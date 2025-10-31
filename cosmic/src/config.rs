use std::any::TypeId;

use cosmic::{
    Application,
    cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry},
    iced::Subscription,
    iced_winit::commands::subsurface::Anchor,
};
use ringboard_sdk::ui_actor::SearchKind;
use serde::{Deserialize, Serialize};

use crate::app::{AppMessage, Model};

#[derive(Deserialize, Serialize, Eq, PartialEq, Copy, Clone, Debug)]
pub enum SerializableSearchKind {
    Plain,
    Regex,
    Mime,
}

#[derive(Deserialize, Serialize, Eq, PartialEq, Copy, Clone, Debug)]
pub enum Position {
    Center,
    Start,
    End,
}

impl From<SerializableSearchKind> for SearchKind {
    fn from(value: SerializableSearchKind) -> Self {
        match value {
            SerializableSearchKind::Plain => SearchKind::Plain,
            SerializableSearchKind::Regex => SearchKind::Regex,
            SerializableSearchKind::Mime => SearchKind::Mime,
        }
    }
}

impl From<SearchKind> for SerializableSearchKind {
    fn from(value: SearchKind) -> Self {
        match value {
            SearchKind::Plain => SerializableSearchKind::Plain,
            SearchKind::Regex => SerializableSearchKind::Regex,
            SearchKind::Mime => SerializableSearchKind::Mime,
        }
    }
}

#[derive(CosmicConfigEntry, Eq, PartialEq, Copy, Clone, Debug)]
#[version = 1]
pub struct Config {
    pub search_kind: SerializableSearchKind,
    pub horizontal_position: Position,
    pub vertical_position: Position,
}

impl Config {
    // TODO what is this for?
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
            search_kind: SearchKind::default().into(),
            horizontal_position: Position::End,
            vertical_position: Position::Start,
        }
    }
}

pub fn config_sub<const APPLET: bool>() -> Subscription<AppMessage> {
    struct ConfigSubscription;
    cosmic_config::config_subscription(
        TypeId::of::<ConfigSubscription>(),
        Model::<APPLET>::APP_ID.into(),
        Config::VERSION,
    )
    .map(|update| AppMessage::ConfigUpdate(update.config))
}
