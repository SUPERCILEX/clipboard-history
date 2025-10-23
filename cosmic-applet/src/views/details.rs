use cosmic::{
    Element,
    iced::{Alignment, Color, Length, Padding},
    iced_widget::text,
    theme::Button,
    widget::{
        button::{self, Catalog, image},
        column, container,
        image::Handle,
        row,
        text::heading,
    },
};

use crate::{
    app::{AppMessage, Entry, EntryData},
    fl, icon,
};

pub fn details_view<'a>(details: Result<&'a Entry, &'a String>) -> Element<'a, AppMessage> {
    let mut header = row()
        .push(
            button::icon(icon!("back"))
                .on_press(AppMessage::CloseDetails)
                .class(button::ButtonClass::AppletIcon),
        )
        .push(container(heading(fl!("details"))).padding(Padding::ZERO.top(2)))
        .align_y(Alignment::Center)
        .spacing(5);

    if let Ok(details) = details {
        header = header.push(actions(details.favorite, details.id));
    }

    let mut column = column().push(header).spacing(10);

    match details {
        Ok(details) => {
            // Mime Type
            match &details.data {
                EntryData::Text { mime, .. }
                | EntryData::HighlightedText { mime, .. }
                | EntryData::Mime(mime)
                | EntryData::Image { mime, .. } => {
                    let row = row()
                        .push(heading(format!("{}:", fl!("mime-type"))))
                        .push(container(text(mime.clone())).padding(Padding::ZERO.top(1)))
                        .padding(Padding::ZERO.left(5))
                        .spacing(5);
                    column = column.push(row);
                }
                _ => (),
            }

            // Content
            let content: Element<AppMessage> = match &details.data {
                EntryData::Text { text: str, .. }
                | EntryData::HighlightedText { text: str, .. } => text(str).into(),
                EntryData::Mime(_) => text(fl!("invalid-mime")).into(),
                EntryData::Loading | EntryData::Image { image: None, .. } => {
                    text(fl!("detail-loading")).into()
                }
                EntryData::Image {
                    image: Some(image_data),
                    ..
                } => {
                    if let Some(data) = image_data.as_rgba8() {
                        container(image(Handle::from_rgba(
                            image_data.width(),
                            image_data.height(),
                            data.to_vec(),
                        )))
                        .align_top(Length::Fill)
                        .align_left(Length::Fill)
                        .into()
                    } else {
                        text(fl!("invalid-image")).into()
                    }
                }
                EntryData::Error(error) => text(format!("{}: {}", fl!("error"), error)).into(),
            };

            column = column.push(
                container(content)
                    .padding(5)
                    .width(Length::Fill)
                    .height(Length::Fill),
            );
        }
        Err(error) => {
            column = column.push(text(error.clone()));
        }
    }

    container(column)
        .height(Length::Fixed(530f32))
        .width(Length::Fixed(400f32))
        .padding(10)
        .into()
}

fn actions(favorite: bool, id: u64) -> Element<'static, AppMessage> {
    let fav_color = Color::from_rgb(1f32, 0.84f32, 0f32); // gold
    let mut fav_class = action_class(fav_color); // gold
    if let Button::Custom { active, .. } = &mut fav_class {
        *active = Box::new(move |focused, theme| {
            let mut style = theme.active(focused, false, &Button::Icon);
            if favorite {
                style.icon_color = Some(fav_color);
            }
            style
        });
    }

    let btns: Element<'static, AppMessage> = row()
        .push(button::icon(icon!("paste")).on_press(AppMessage::Paste(id)))
        .push(
            button::icon(icon!("star"))
                .on_press(AppMessage::ChangeFavorite(id, favorite))
                .class(fav_class),
        )
        .push(
            button::icon(icon!("trash"))
                .on_press(AppMessage::Delete(id))
                .class(action_class(Color::from_rgb(0.8f32, 0.2f32, 0.2f32))), // red
        )
        .into();

    container(btns).align_right(Length::Fill).into()
}

fn action_class(color: Color) -> Button {
    Button::Custom {
        active: Box::new(move |focused, theme| button::Style {
            ..theme.active(focused, false, &Button::Icon)
        }),
        disabled: Box::new(move |theme| theme.disabled(&Button::Icon)),
        hovered: Box::new(move |focused, theme| button::Style {
            icon_color: Some(color),
            ..theme.hovered(focused, false, &Button::Icon)
        }),
        pressed: Box::new(move |focused, theme| button::Style {
            icon_color: Some(color),
            ..theme.pressed(focused, false, &Button::Icon)
        }),
    }
}
