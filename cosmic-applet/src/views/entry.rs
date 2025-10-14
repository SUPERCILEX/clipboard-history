use cosmic::{
    Element,
    iced::Length,
    theme::Button,
    widget::{
        button::{self, Catalog},
        text,
    },
};
use ringboard_sdk::ui_actor::{UiEntry, UiEntryCache};

use crate::app::AppMessage;

pub fn entry_view(entry: &UiEntry, is_focused: bool) -> Element<'_, AppMessage> {
    let content = if let UiEntryCache::Text { one_liner }
    | UiEntryCache::HighlightedText { one_liner, .. } = &entry.cache
    {
        one_liner
    } else {
        println!("Entry without text cache: {:?}", entry);
        "<loading...>"
    };

    let btn = button::custom(text(content.to_string()))
        .on_press(AppMessage::Paste(entry.entry.id()))
        .padding([8, 16])
        .class(Button::Custom {
            active: Box::new(move |focused, theme| {
                let rad_s = theme.cosmic().corner_radii.radius_s;
                let focused = is_focused || focused;

                let a = if focused {
                    theme.hovered(focused, focused, &Button::Text)
                } else {
                    theme.hovered(focused, focused, &Button::Standard)
                };

                button::Style {
                    border_radius: rad_s.into(),
                    outline_width: 0.0,
                    ..a
                }
            }),
            disabled: Box::new(move |theme| theme.disabled(&Button::Text)),
            hovered: Box::new(move |focused, theme| {
                let rad_s = theme.cosmic().corner_radii.radius_s;
                let focused = is_focused || focused;

                let text = theme.hovered(focused, focused, &Button::Text);
                button::Style {
                    border_radius: rad_s.into(),
                    outline_width: 0.0,
                    ..text
                }
            }),
            pressed: Box::new(move |focused, theme| {
                let rad_s = theme.cosmic().corner_radii.radius_s;
                let focused = is_focused || focused;

                let text = theme.pressed(focused, focused, &Button::Text);
                button::Style {
                    border_radius: rad_s.into(),
                    outline_width: 0.0,
                    ..text
                }
            }),
        });

    let btn: Element<_> = btn.width(Length::Fill).into();

    btn
}
