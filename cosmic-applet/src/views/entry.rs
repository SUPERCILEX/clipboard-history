use cosmic::{
    Element,
    iced::{Color, Length},
    iced_widget::{hover, rich_text, span},
    theme::{Button, Container},
    widget::{
        MouseArea, RcElementWrapper,
        button::{self, Catalog},
        container, context_menu,
        menu::Tree,
        row, text,
    },
};
use ringboard_sdk::ui_actor::{UiEntry, UiEntryCache};

use crate::{app::AppMessage, icon};

pub fn entry_view<'a>(
    entry: &'a UiEntry,
    favorite: bool,
    theme: &'a cosmic::Theme,
) -> Element<'a, AppMessage> {
    let content: Element<'_, AppMessage> = match &entry.cache {
        UiEntryCache::Text { one_liner } => text(one_liner.to_string()).into(),
        UiEntryCache::HighlightedText {
            one_liner,
            start,
            end,
        } => {
            let pre = &one_liner[..*start];
            let highlighted = &one_liner[*start..*end];
            let post = &one_liner[*end..];

            let color = theme.cosmic().accent_color();
            let text = rich_text![pre, span(highlighted).color(color), post];

            text.into()
        }
        _ => {
            println!("Entry without highlighted text cache: {:?}", entry.entry);
            text("< loading... >").into()
        }
    };

    let btn = button::custom(content)
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

    let overlay: Element<'static, AppMessage> = row()
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
        )
        .into();

    let tree = Tree::new(RcElementWrapper::new(overlay));
    //context_menu(btn, Some(vec![tree])).into()
    MouseArea::new(btn)
        .on_right_press(AppMessage::ViewDetails(entry.entry.id()))
        .into()
}
