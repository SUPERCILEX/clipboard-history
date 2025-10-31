use cosmic::{
    Element,
    iced::{Alignment, Length},
    widget::{
        column, container,
        segmented_button::{Entity, SingleSelectModel},
        segmented_control,
        text::heading,
    },
};

use crate::{
    app::AppMessage,
    config::{Config, Position, SerializableSearchKind},
    fl,
};

pub fn settings_view<'a>(
    filter_mode_model: &'a SingleSelectModel,
    horizontal_pos_model: &'a SingleSelectModel,
    vertical_pos_model: &'a SingleSelectModel,
) -> Element<'a, AppMessage> {
    let search_mode = select(
        fl!("filter-mode-heading"),
        filter_mode_model,
        AppMessage::SelectFilterMode,
    );

    let horizontal_position = select(
        fl!("horizontal-position-heading"),
        horizontal_pos_model,
        AppMessage::SelectHorizontalPosition,
    );

    let vertical_position = select(
        fl!("vertical-position-heading"),
        vertical_pos_model,
        AppMessage::SelectVerticalPosition,
    );

    let view: Element<_> = column()
        .push(search_mode)
        .push(vertical_position)
        .push(horizontal_position)
        .spacing(20)
        .align_x(Alignment::Center)
        .padding(10)
        .into();

    container(view)
        .height(Length::Fixed(255f32))
        .width(Length::Fixed(400f32))
        .into()
}

fn select<'a, F>(text: String, model: &'a SingleSelectModel, f: F) -> Element<'a, AppMessage>
where
    F: 'static + Fn(Entity) -> AppMessage,
{
    let heading = heading(text);

    let select = segmented_control::horizontal(model).on_activate(f);

    column()
        .push(heading)
        .push(select)
        .spacing(10)
        .align_x(Alignment::Center)
        .width(Length::Fill)
        .into()
}

pub fn filter_mode_model(config: &Config) -> SingleSelectModel {
    let mut filter_mode_model = SingleSelectModel::default();
    let plain = filter_mode_model
        .insert()
        .text(fl!("filter-mode-plain"))
        .data(SerializableSearchKind::Plain)
        .id();
    let regex = filter_mode_model
        .insert()
        .text(fl!("filter-mode-regex"))
        .data(SerializableSearchKind::Regex)
        .id();
    let mime = filter_mode_model
        .insert()
        .text(fl!("filter-mode-mime"))
        .data(SerializableSearchKind::Mime)
        .id();

    match config.search_kind {
        SerializableSearchKind::Plain => filter_mode_model.activate(plain),
        SerializableSearchKind::Regex => filter_mode_model.activate(regex),
        SerializableSearchKind::Mime => filter_mode_model.activate(mime),
    }

    filter_mode_model
}

pub fn horizontal_position_model(config: &Config) -> SingleSelectModel {
    let mut horizontal_position_model = SingleSelectModel::default();
    let start = horizontal_position_model
        .insert()
        .text(fl!("position-left"))
        .data(Position::Start)
        .id();
    let center = horizontal_position_model
        .insert()
        .text(fl!("position-center"))
        .data(Position::Center)
        .id();
    let end = horizontal_position_model
        .insert()
        .text(fl!("position-right"))
        .data(Position::End)
        .id();

    match config.horizontal_position {
        Position::Start => horizontal_position_model.activate(start),
        Position::Center => horizontal_position_model.activate(center),
        Position::End => horizontal_position_model.activate(end),
    }

    horizontal_position_model
}

pub fn vertical_position_model(config: &Config) -> SingleSelectModel {
    let mut vertical_position_model = SingleSelectModel::default();
    let start = vertical_position_model
        .insert()
        .text(fl!("position-top"))
        .data(Position::Start)
        .id();
    let center = vertical_position_model
        .insert()
        .text(fl!("position-center"))
        .data(Position::Center)
        .id();
    let end = vertical_position_model
        .insert()
        .text(fl!("position-bottom"))
        .data(Position::End)
        .id();

    match config.vertical_position {
        Position::Start => vertical_position_model.activate(start),
        Position::Center => vertical_position_model.activate(center),
        Position::End => vertical_position_model.activate(end),
    }

    vertical_position_model
}
