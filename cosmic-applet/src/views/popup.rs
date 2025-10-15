use cosmic::{
    Apply, Element, Theme,
    iced::{Alignment, Length, padding},
    widget::{column, container, horizontal_space, row, scrollable, search_input, text::heading},
};
use ringboard_sdk::ui_actor::UiEntry;

use crate::{app::AppMessage, fl, views::entry::entry_view};

pub fn popup_view<'a>(
    entries: &'a [UiEntry],
    favorites: &'a [UiEntry],
    search: &'a str,
    theme: &'a Theme,
) -> Element<'a, AppMessage> {
    let search = container(
        row()
            .push(
                search_input(fl!("search-placeholder"), search)
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
        let mut column = column();
        if !favorites.is_empty() {
            let fav_section = list_section(favorites, fl!("favorites-heading"), true, theme);
            column = column.push(fav_section);
        }
        if !entries.is_empty() {
            if !favorites.is_empty() {
                column = column.push(horizontal_space().height(5));
            }
            let others_section = list_section(entries, fl!("history-heading"), false, theme);
            column = column.push(others_section);
        }

        if favorites.is_empty() && entries.is_empty() {
            //column.push(caption("No items found").style(cosmic::theme::Text::Disabled));
        }

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

fn list_section<'a>(
    ui_entries: &'a [UiEntry],
    name: String,
    favoirte: bool,
    theme: &'a Theme,
) -> Element<'a, AppMessage> {
    let mut entries = vec![heading(name).into()];
    entries.extend(
        ui_entries
            .iter()
            .map(|entry| entry_view(entry, favoirte, theme)),
    );

    let column = column::with_children(entries)
        .spacing(5f32)
        .padding(padding::right(10));

    column.into()
}
