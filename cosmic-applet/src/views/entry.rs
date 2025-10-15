use cosmic::{
    Element,
    iced::{Color, Length},
    iced_widget::hover,
    theme::{Button, Container},
    widget::{
        button::{self, Catalog},
        container, row, text,
    },
};
use ringboard_sdk::ui_actor::{UiEntry, UiEntryCache};

use crate::{app::AppMessage, icon};

pub fn entry_view(entry: &UiEntry, favorite: bool) -> Element<'_, AppMessage> {
    let content = if let UiEntryCache::Text { one_liner }
    | UiEntryCache::HighlightedText { one_liner, .. } = &entry.cache
    {
        one_liner
    } else {
        println!("Entry without text cache: {:?}", entry.entry);
        "<loading...>"
    };

    let btn = button::custom(text(content.to_string()))
        .on_press(AppMessage::Paste(entry.entry.id()))
        .padding([8, 16])
        .width(Length::Fill)
        .class(Button::Custom {
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
        });

    let overlay = container(
        container(
            row()
                .push(
                    button::icon(icon!("star"))
                        .on_press(AppMessage::ChangeFavorite(entry.entry.id(), favorite))
                        .class(Button::Custom {
                            active: Box::new(move |focused, theme| {
                                let mut style = theme.active(focused, false, &Button::Icon);
                                if favorite {
                                    style.icon_color = Some(Color::from_rgb(1f32, 0.84f32, 0f32)); // gold
                                }
                                style
                            }),
                            disabled: Box::new(move |theme| theme.disabled(&Button::Icon)),
                            hovered: Box::new(move |focused, theme| button::Style {
                                icon_color: Some(Color::from_rgb(1f32, 0.84f32, 0f32)), // gold
                                ..theme.hovered(focused, false, &Button::Icon)
                            }),
                            pressed: Box::new(move |focused, theme| button::Style {
                                icon_color: Some(Color::from_rgb(1f32, 0.84f32, 0f32)), // gold
                                ..theme.pressed(focused, false, &Button::Icon)
                            }),
                        }),
                )
                .push(
                    button::icon(icon!("trash"))
                        .on_press(AppMessage::Delete(entry.entry.id()))
                        .class(Button::Custom {
                            active: Box::new(move |focused, theme| button::Style {
                                ..theme.active(focused, false, &Button::Icon)
                            }),
                            disabled: Box::new(move |theme| theme.disabled(&Button::Icon)),
                            hovered: Box::new(move |focused, theme| button::Style {
                                icon_color: Some(Color::from_rgb(0.8f32, 0.2f32, 0.2f32)), // red
                                ..theme.hovered(focused, false, &Button::Icon)
                            }),
                            pressed: Box::new(move |focused, theme| button::Style {
                                icon_color: Some(Color::from_rgb(0.8f32, 0.2f32, 0.2f32)), // red
                                ..theme.pressed(focused, false, &Button::Icon)
                            }),
                        }),
                ),
        )
        .class(Container::Card),
    )
    .align_right(Length::Fill)
    .padding([1, 0, 0, 4]);

    hover(btn, overlay)
}
