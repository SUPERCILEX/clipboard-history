use cosmic::{
    Element, Task,
    iced_widget::{Column, button, column, row},
    theme::{self, Button},
    widget::{self},
};
use ringboard_sdk::core::protocol::RingKind;

use crate::app::App;

#[derive(Debug, Clone)]
pub enum Message {
    ChangeEntriesLimit(Option<u32>),
    ToggleShowFavourites(bool),
    // Not ideal
    ChangeMainRings,
}

#[derive(Debug)]
pub struct Settings;

impl Settings {
    pub fn new() -> Self {
        Self
    }

    pub fn view(&self, app: &App) -> Element<'static, Message> {
        let header = row![
            widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                .on_press(Message::ChangeMainRings)
                .label("Back")
                .spacing(theme::spacing().space_xxs)
                .class(Button::Link)
        ];

        let settings = column![
            widget::settings::item(
                "Show favourites",
                widget::toggler(app.config.show_favourites)
                    .on_toggle(Message::ToggleShowFavourites),
            ),
            widget::settings::item(
                "Entries limit",
                widget::text_input("limit", app.config.items_max.to_string()).on_input(|v| {
                    if v == "" {
                        return Message::ChangeEntriesLimit(Some(0));
                    }

                    if let Ok(v) = v.parse::<u32>() {
                        Message::ChangeEntriesLimit(Some(v))
                    } else {
                        Message::ChangeEntriesLimit(None)
                    }
                }),
            )
        ].spacing(theme::spacing().space_xs);

        let content = column![header, settings].spacing(theme::spacing().space_m);

        content.into()
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            _ => Task::none(),
        }
    }
}
