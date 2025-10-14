use cosmic::{
    Apply, Element,
    iced::{Alignment, Length, padding},
    widget::{column, container, horizontal_space, row, scrollable, search_input},
};
use ringboard_sdk::ui_actor::UiEntry;

use crate::{app::AppMessage, views::entry::entry_view};

pub fn popup_view<'a>(entries: &'a [UiEntry], search: &'a str) -> Element<'a, AppMessage> {
    let search = container(
        row()
            .push(
                search_input("search", search)
                    .always_active()
                    .on_input(AppMessage::Search)
                    .on_paste(AppMessage::Search)
                    .on_clear(AppMessage::Search("".into()))
                    .width(Length::Fill),
            )
            .push(horizontal_space().width(5)),
    )
    .padding(padding::all(15f32).bottom(0));

    let list_view = container({
        let entries: Vec<_> = entries
            .iter()
            .take(50)
            .map(|entry| entry_view(entry, false))
            .collect();
        let column = column::with_children(entries)
            .spacing(5f32)
            .padding(padding::right(10));
        scrollable(column).apply(Element::from)
    })
    .padding(padding::all(20).top(0));

    let view: Element<_> = column()
        .push(search)
        .push(list_view)
        .spacing(20)
        .align_x(Alignment::Center)
        .into();

    container(view)
        .height(Length::Fixed(530f32))
        .width(Length::Fixed(400f32))
        .into()
}
