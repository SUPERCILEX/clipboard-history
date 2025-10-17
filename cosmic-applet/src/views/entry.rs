use cosmic::{
    Element,
    iced::Length,
    iced_widget::{rich_text, span},
    theme::Button,
    widget::{
        MouseArea,
        button::{self, Catalog},
        text,
    },
};

use crate::app::{AppMessage, Entry, EntryData};

pub fn entry_view<'a>(
    entry: &'a Entry,
    favorite: bool,
    theme: &'a cosmic::Theme,
) -> Element<'a, AppMessage> {
    let content: Element<'_, AppMessage> = match &entry.data {
        EntryData::Text { text: str, .. } => text(str).into(),
        EntryData::HighlightedText {
            text: str,
            start,
            end,
            ..
        } => {
            let pre = &str[..*start];
            let highlighted = &str[*start..*end];
            let post = &str[*end..];

            let color = theme.cosmic().accent_color();
            let text = rich_text![pre, span(highlighted).color(color), post];

            text.into()
        }
        _ => {
            println!("Entry without highlighted text cache: {:?}", entry.id);
            text("< loading... >").into()
        }
    };

    let btn = button::custom(content)
        .on_press(AppMessage::Paste(entry.id))
        .padding([8, 16])
        .width(Length::Fill)
        .class(entry_class());

    MouseArea::new(btn)
        .on_right_press(AppMessage::ViewDetails(entry.id, favorite))
        .into()
}

pub fn entry_class() -> Button {
    Button::Custom {
        active: Box::new(move |focused, theme| {
            let rad_s = theme.cosmic().corner_radii.radius_s;

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

            let text = theme.hovered(focused, focused, &Button::Text);
            button::Style {
                border_radius: rad_s.into(),
                outline_width: 0.0,
                ..text
            }
        }),
        pressed: Box::new(move |focused, theme| {
            let rad_s = theme.cosmic().corner_radii.radius_s;

            let text = theme.pressed(focused, focused, &Button::Text);
            button::Style {
                border_radius: rad_s.into(),
                outline_width: 0.0,
                ..text
            }
        }),
    }
}
