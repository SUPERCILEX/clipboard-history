use cosmic::{
    Element,
    iced::{Alignment, Length},
    theme::Button,
    widget::{button, column, container, row, text::heading},
};

use crate::{
    app::AppMessage,
    config::{Config, FilterMode},
    fl,
};

pub fn settings_view<'a>(config: &'a Config) -> Element<'a, AppMessage> {
    let search_mode_heading = heading(fl!("filter-mode-heading"));

    let search_mode_select = row()
        .push(search_mode_btn(FilterMode::Plain, config.filter_mode))
        .push(search_mode_btn(FilterMode::Regex, config.filter_mode))
        .push(search_mode_btn(FilterMode::Mime, config.filter_mode))
        .spacing(10);

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
        .height(Length::Fixed(90f32))
        .width(Length::Fixed(320f32))
        .into()
}

fn search_mode_btn<'a>(mode: FilterMode, selected: FilterMode) -> Element<'a, AppMessage> {
    let is_selected = mode == selected;
    let label = match mode {
        FilterMode::Plain => fl!("filter-mode-plain"),
        FilterMode::Regex => fl!("filter-mode-regex"),
        FilterMode::Mime => fl!("filter-mode-mime"),
    };

    let mut btn = button::text(label).on_press(AppMessage::SelectFilterMode(mode));

    if is_selected {
        btn = btn.class(Button::Suggested);
    } else {
        btn = btn.class(Button::Standard);
    }

    btn.into()
}
