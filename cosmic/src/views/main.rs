use cosmic::{Element, Task};

use crate::app::App;

use super::{rings::{self, Rings}, settings::{self, Settings}};

#[derive(Debug, Clone)]
pub enum Message {
    Settings(settings::Message),
    Rings(rings::Message),
    ChangeMain(MainRoute),
}

#[derive(Debug, Clone)]
pub enum MainRoute {
    Settings,
    Rings,
}

#[derive(Debug, Clone)]
pub enum Main {
    Settings(Settings),
    Rings(Rings),
}

impl Default for Main {
    fn default() -> Self {
        Self::Rings(Rings::default())
    }
}

pub trait View<Message> {
    fn view(&self, app: &App) -> Element<Message>;
    fn update(&mut self, message: Message) -> Task<Message>;
}