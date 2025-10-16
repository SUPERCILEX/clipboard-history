use cosmic::{
    Element,
    iced::{Alignment, Length},
    widget::{
        column, container, segmented_button::SingleSelectModel, segmented_control, text::heading,
    },
};

use crate::{
    app::AppMessage,
    config::{Config, FilterMode},
    fl,
};

pub fn settings_view<'a>(filter_mode_model: &'a SingleSelectModel) -> Element<'a, AppMessage> {
    let search_mode_heading = heading(fl!("filter-mode-heading"));

    let search_mode_select = segmented_control::horizontal(&filter_mode_model)
        .on_activate(|e| AppMessage::SelectFilterMode(e));

    let search_mode = column()
        .push(search_mode_heading)
        .push(search_mode_select)
        .spacing(10)
        .align_x(Alignment::Center)
        .width(Length::Fill);

    let view: Element<_> = column()
        .push(search_mode)
        .spacing(20)
        .align_x(Alignment::Center)
        .padding(10)
        .into();

    container(view)
        .height(Length::Fixed(85f32))
        .width(Length::Fixed(400f32))
        .into()
}

pub fn filter_mode_model(config: &Config) -> SingleSelectModel {
    let mut filter_mode_model = SingleSelectModel::default();
    let plain = filter_mode_model
        .insert()
        .text(fl!("filter-mode-plain"))
        .data(FilterMode::Plain)
        .id();
    let regex = filter_mode_model
        .insert()
        .text(fl!("filter-mode-regex"))
        .data(FilterMode::Regex)
        .id();
    let mime = filter_mode_model
        .insert()
        .text(fl!("filter-mode-mime"))
        .data(FilterMode::Mime)
        .id();

    match config.filter_mode {
        FilterMode::Plain => filter_mode_model.activate(plain),
        FilterMode::Regex => filter_mode_model.activate(regex),
        FilterMode::Mime => filter_mode_model.activate(mime),
    }

    filter_mode_model
}
