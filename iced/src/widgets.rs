use iced::{
    Alignment, Element, Length, Padding,
    widget::{
        Space, button, column, container, image, mouse_area, row, scrollable, text, text_input,
        tooltip,
    },
};
use ringboard_sdk::core::protocol::RingKind;
use ringboard_sdk::ui_actor::{DetailedEntry, UiEntry, UiEntryCache};

use crate::app::RingboardApp;
use crate::message::Message;
use crate::state::ActiveTab;
use crate::theme::{
    badge_style, card_style, danger_button_style, detail_panel_style, error_banner_style,
    icon_button_style, primary_button_style, search_bar_style, secondary_button_style,
    section_header_style, warning_banner_style,
};

// ------------------------------------------------------------------
// Top-level view
// ------------------------------------------------------------------

pub fn main_view(app: &RingboardApp) -> Element<'_, Message> {
    let show_sections = app.state.ui.query.is_empty() && app.state.ui.active_tab == ActiveTab::All;
    let detail_id = app.state.ui.details_requested;
    let radius = app.state.theme.border_radius();
    let has_entries = !app.filtered_entries().is_empty();

    let mut col = column![search_bar(app), tab_bar(app)].spacing(8);

    if let Some(ref e) = app.state.ui.fatal_error {
        col = col.push(fatal_error_banner(app, e, radius));
    }

    if let Some(ref e) = app.state.ui.last_error {
        col = col.push(error_banner(app, e, radius));
    }

    let content: Element<Message> = if !has_entries && app.state.ui.pending_search_token.is_none() {
        empty_state(app)
    } else {
        entry_list(app, detail_id)
    };
    col = col.push(content);

    if show_sections && has_entries {
        col = col.push(fast_paste_bar(app, radius));
    }

    col = col.push(status_bar(app, radius));

    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(app.state.theme.input_padding())
        .into()
}

// ------------------------------------------------------------------
// Search bar
// ------------------------------------------------------------------

fn search_bar(app: &RingboardApp) -> Element<'_, Message> {
    let hint = match app.state.ui.search_kind {
        ringboard_sdk::ui_actor::SearchKind::Plain => "Search clipboard history...",
        ringboard_sdk::ui_actor::SearchKind::Regex => "Regex search...",
        ringboard_sdk::ui_actor::SearchKind::Mime => "MIME type search...",
    };
    let kind_label = match app.state.ui.search_kind {
        ringboard_sdk::ui_actor::SearchKind::Plain => "Text",
        ringboard_sdk::ui_actor::SearchKind::Regex => "Regex",
        ringboard_sdk::ui_actor::SearchKind::Mime => "MIME",
    };
    let radius = app.state.theme.border_radius();
    let mono = app.state.theme.mono_font();

    let input = text_input(hint, &app.state.ui.query)
        .on_input(Message::SearchChanged)
        .width(Length::Fill)
        .size(app.state.theme.font_size())
        .font(mono);

    let kind_button = tooltip(
        button(text(kind_label).size(12).font(mono))
            .on_press(Message::SearchKindToggled)
            .style(secondary_button_style)
            .padding(app.state.theme.button_padding()),
        text(format!("Search kind: {} (Alt+X/M)", kind_label)).size(12),
        tooltip::Position::Bottom,
    );

    container(
        row![input, kind_button]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding(app.state.theme.input_padding())
    .style(move |theme: &iced::Theme| search_bar_style(theme, radius))
    .into()
}

// ------------------------------------------------------------------
// Tab bar
// ------------------------------------------------------------------

fn tab_bar(app: &RingboardApp) -> Element<'_, Message> {
    let current = app.state.ui.active_tab;
    let radius = app.state.theme.border_radius();
    let font = app.state.theme.font();

    let buttons: Vec<Element<Message>> = ActiveTab::ALL
        .iter()
        .map(|tab| {
            let label = tab.label();
            let is_active = current == *tab;
            let btn = button(text(label).size(13).font(font))
                .padding(app.state.theme.button_padding())
                .style(move |theme, status| {
                    if is_active {
                        primary_button_style(theme, status)
                    } else {
                        secondary_button_style(theme, status)
                    }
                })
                .on_press(Message::TabSelected(*tab));
            tooltip(
                btn,
                text(format!("Alt+{}", label)).size(11),
                tooltip::Position::Bottom,
            )
            .into()
        })
        .collect();

    container(row(buttons).spacing(4).align_y(Alignment::Center))
        .padding(Padding::new(4.0))
        .style(move |theme: &iced::Theme| search_bar_style(theme, radius))
        .into()
}

// ------------------------------------------------------------------
// Entry list
// ------------------------------------------------------------------

fn entry_list<'a>(app: &'a RingboardApp, detail_id: Option<u64>) -> Element<'a, Message> {
    let filtered = app.filtered_entries();
    let show_sections = app.state.ui.query.is_empty() && app.state.ui.active_tab == ActiveTab::All;
    let pinned: Vec<&UiEntry> = filtered
        .iter()
        .filter(|e| e.entry.ring() == RingKind::Favorites)
        .copied()
        .collect();
    let unpinned: Vec<&UiEntry> = filtered
        .iter()
        .filter(|e| e.entry.ring() == RingKind::Main)
        .copied()
        .collect();
    let has_favorites = !pinned.is_empty();

    let mut col = column![].spacing(4);

    if show_sections && has_favorites {
        col = col.push(section_header(
            app,
            "Pinned",
            pinned.len(),
            true,
            app.state.ui.pinned_expanded,
        ));

        if app.state.ui.pinned_expanded {
            for entry in &pinned {
                col = col.push(entry_card(app, entry, detail_id));
            }
        }

        if !unpinned.is_empty() {
            col = col.push(section_header(app, "Recent", unpinned.len(), false, true));
        }
    }

    let render_items: Vec<&UiEntry> = if show_sections { unpinned } else { filtered };

    for entry in render_items {
        col = col.push(entry_card(app, entry, detail_id));
    }

    scrollable(col)
        .id(iced::widget::Id::new("entry-list"))
        .height(Length::Fill)
        .into()
}

// ------------------------------------------------------------------
// Section header
// ------------------------------------------------------------------

fn section_header<'a>(
    app: &'a RingboardApp,
    title: &'a str,
    count: usize,
    collapsible: bool,
    expanded: bool,
) -> Element<'a, Message> {
    let radius = app.state.theme.border_radius();
    let _palette = app.state.theme.extended_palette();

    let title_row: Element<Message> = if collapsible {
        let arrow = if expanded { "\u{25BC}" } else { "\u{25B6}" };
        row![
            button(text(arrow).size(12))
                .style(icon_button_style)
                .padding(4)
                .on_press(Message::PinnedToggled),
            text(title).size(13).font(app.state.theme.font()),
            badge(app, count.to_string(), radius, true),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    } else {
        row![
            text(title).size(13).font(app.state.theme.font()),
            badge(app, count.to_string(), radius, false),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    };

    container(title_row)
        .style(move |theme: &iced::Theme| section_header_style(theme, radius))
        .padding(app.state.theme.input_padding())
        .width(Length::Fill)
        .into()
}

fn badge<'a>(
    app: &'a RingboardApp,
    label: String,
    radius: f32,
    primary: bool,
) -> Element<'a, Message> {
    container(text(label).size(11).font(app.state.theme.font()))
        .style(move |theme: &iced::Theme| badge_style(theme, radius, primary))
        .padding(Padding::new(2.0).left(8).right(8))
        .width(Length::Shrink)
        .into()
}

// ------------------------------------------------------------------
// Entry card
// ------------------------------------------------------------------

fn entry_card<'a>(
    app: &'a RingboardApp,
    entry: &'a UiEntry,
    detail_id: Option<u64>,
) -> Element<'a, Message> {
    let id = entry.entry.id();
    let is_favorite = entry.entry.ring() == RingKind::Favorites;
    let is_highlighted = app.current_highlight_id() == Some(id);
    let is_detail_open = detail_id == Some(id);
    let radius = app.state.theme.border_radius();

    let preview = content_preview(app, entry, id);
    let actions = action_row(id, is_favorite, is_detail_open);

    let mut card_col = column![
        row![preview, Space::new().width(Length::Fill), actions]
            .spacing(8)
            .align_y(Alignment::Center),
    ]
    .spacing(6);

    if is_detail_open {
        card_col = card_col.push(detail_panel(app, entry, id, radius));
    }

    mouse_area(
        container(card_col)
            .style(move |theme: &iced::Theme| {
                card_style(theme, is_highlighted, is_favorite, radius)
            })
            .padding(app.state.theme.input_padding())
            .width(Length::Fill),
    )
    .on_press(Message::EntryClicked(id))
    .into()
}

fn content_preview<'a>(app: &'a RingboardApp, entry: &'a UiEntry, id: u64) -> Element<'a, Message> {
    match &entry.cache {
        UiEntryCache::Text { one_liner } | UiEntryCache::HighlightedText { one_liner, .. } => {
            let display = if one_liner.len() > 200 {
                &one_liner[..200]
            } else {
                one_liner
            };
            column![
                text(display)
                    .font(app.state.theme.mono_font())
                    .size(app.state.theme.mono_font_size())
                    .width(Length::Fill)
            ]
            .spacing(0)
            .padding(0)
            .into()
        }
        UiEntryCache::Image => {
            if let Some(handle) = app.image_cache.get(&id) {
                row![
                    image(handle.clone())
                        .width(Length::Fixed(80.0))
                        .height(Length::Fixed(80.0)),
                    text("Image").size(13).font(app.state.theme.font()),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into()
            } else {
                // View must not mutate state, so we cannot send LoadImage here.
                // The detail panel or periodic poll will request the image.
                row![
                    text("Image (loading...)")
                        .size(13)
                        .font(app.state.theme.font())
                ]
                .into()
            }
        }
        UiEntryCache::Binary { mime_type } => column![
            text(format!("[{}]", mime_type))
                .size(13)
                .font(app.state.theme.mono_font())
        ]
        .spacing(0)
        .padding(0)
        .into(),
        UiEntryCache::Error(_) => column![text("Error").size(13).font(app.state.theme.font())]
            .spacing(0)
            .padding(0)
            .into(),
    }
}

fn action_row(id: u64, is_favorite: bool, is_detail_open: bool) -> Element<'static, Message> {
    let star = if is_favorite { "\u{2605}" } else { "\u{2606}" };
    let detail_arrow = if is_detail_open {
        "\u{25B2}"
    } else {
        "\u{25BC}"
    };

    row![
        tooltip(
            button(text(star).size(16))
                .on_press(Message::FavoriteToggled(id))
                .style(icon_button_style)
                .padding(6),
            text(if is_favorite {
                "Unfavorite"
            } else {
                "Favorite"
            })
            .size(11),
            tooltip::Position::Top,
        ),
        tooltip(
            button(text("\u{2715}").size(14))
                .on_press(Message::DeleteEntry(id))
                .style(danger_button_style)
                .padding(6),
            text("Delete").size(11),
            tooltip::Position::Top,
        ),
        tooltip(
            button(text(detail_arrow).size(12))
                .on_press(if is_detail_open {
                    Message::DetailClosed
                } else {
                    Message::DetailRequested(id)
                })
                .style(icon_button_style)
                .padding(6),
            text(if is_detail_open {
                "Hide details"
            } else {
                "Show details"
            })
            .size(11),
            tooltip::Position::Top,
        ),
    ]
    .spacing(2)
    .align_y(Alignment::Center)
    .into()
}

// ------------------------------------------------------------------
// Detail panel
// ------------------------------------------------------------------

fn detail_panel<'a>(
    app: &'a RingboardApp,
    entry: &'a UiEntry,
    id: u64,
    radius: f32,
) -> Element<'a, Message> {
    let content: Element<Message> = match &app.state.ui.detailed_entry {
        None => text("Loading details...")
            .size(13)
            .font(app.state.theme.font())
            .into(),
        Some(DetailedEntry {
            mime_type,
            full_text,
        }) => {
            let mut col = column![].spacing(6);

            if !mime_type.is_empty() {
                col = col.push(
                    row![
                        text("MIME:").size(12).font(app.state.theme.font()),
                        text(mime_type.as_ref())
                            .size(12)
                            .font(app.state.theme.mono_font()),
                    ]
                    .spacing(6),
                );
            }

            if let Some(full) = full_text {
                col = col.push(
                    scrollable(
                        text(&**full)
                            .font(app.state.theme.mono_font())
                            .size(app.state.theme.mono_font_size())
                            .width(Length::Fill),
                    )
                    .height(Length::Fixed(200.0)),
                );
            } else if matches!(entry.cache, UiEntryCache::Image) {
                if let Some(handle) = app.image_cache.get(&id) {
                    col = col.push(
                        scrollable(image(handle.clone()).width(Length::Fill))
                            .height(Length::Fixed(300.0)),
                    );
                }
            } else {
                col = col.push(text("Binary data").size(12).font(app.state.theme.font()));
            }

            col.into()
        }
    };

    container(content)
        .style(move |theme: &iced::Theme| detail_panel_style(theme, radius))
        .padding(app.state.theme.input_padding())
        .width(Length::Fill)
        .into()
}

// ------------------------------------------------------------------
// Empty state
// ------------------------------------------------------------------

fn empty_state(app: &RingboardApp) -> Element<'_, Message> {
    container(
        column![
            text("Nothing to see here")
                .size(18)
                .font(app.state.theme.font()),
            text("Try a different search or tab")
                .size(13)
                .font(app.state.theme.font()),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .into()
}

// ------------------------------------------------------------------
// Error banners
// ------------------------------------------------------------------

fn fatal_error_banner<'a>(
    app: &'a RingboardApp,
    error: &'a ringboard_sdk::ClientError,
    radius: f32,
) -> Element<'a, Message> {
    container(
        column![
            text("Fatal error").size(14).font(app.state.theme.font()),
            text(format!("{error}"))
                .size(12)
                .font(app.state.theme.mono_font()),
        ]
        .spacing(4),
    )
    .style(move |theme: &iced::Theme| error_banner_style(theme, radius))
    .padding(app.state.theme.input_padding())
    .width(Length::Fill)
    .into()
}

fn error_banner<'a>(
    app: &'a RingboardApp,
    error: &'a ringboard_sdk::ui_actor::CommandError,
    radius: f32,
) -> Element<'a, Message> {
    container(
        row![
            column![
                text("Error").size(12).font(app.state.theme.font()),
                text(format!("{error:#?}"))
                    .size(11)
                    .font(app.state.theme.mono_font()),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(text("\u{2715}").size(12))
                .style(icon_button_style)
                .padding(6)
                .on_press(Message::DismissError),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .style(move |theme: &iced::Theme| warning_banner_style(theme, radius))
    .padding(app.state.theme.input_padding())
    .width(Length::Fill)
    .into()
}

// ------------------------------------------------------------------
// Fast paste bar
// ------------------------------------------------------------------

fn fast_paste_bar<'a>(app: &'a RingboardApp, radius: f32) -> Element<'a, Message> {
    let nav = app.nav_entries();
    let chips: Vec<Element<Message>> = nav
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, entry)| {
            let id = entry.entry.id();
            let label = format!("{}", i);
            tooltip(
                button(text(label).size(11).font(app.state.theme.mono_font()))
                    .on_press(Message::FastPaste(id))
                    .style(move |theme, status| {
                        if i == 0 {
                            primary_button_style(theme, status)
                        } else {
                            secondary_button_style(theme, status)
                        }
                    })
                    .padding(Padding::new(4.0).left(8).right(8)),
                text(format!("Ctrl+{} to paste", i)).size(11),
                tooltip::Position::Top,
            )
            .into()
        })
        .collect();

    if chips.is_empty() {
        return Space::new().into();
    }

    container(
        row![
            text("Fast paste:").size(11).font(app.state.theme.font()),
            row(chips).spacing(4),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .style(move |theme: &iced::Theme| search_bar_style(theme, radius))
    .padding(Padding::new(6.0))
    .width(Length::Fill)
    .into()
}

// ------------------------------------------------------------------
// Status bar
// ------------------------------------------------------------------

fn status_bar<'a>(app: &'a RingboardApp, radius: f32) -> Element<'a, Message> {
    let filtered = app.filtered_entries();
    let pinned: Vec<&UiEntry> = filtered
        .iter()
        .filter(|e| e.entry.ring() == RingKind::Favorites)
        .copied()
        .collect();
    let unpinned: Vec<&UiEntry> = filtered
        .iter()
        .filter(|e| e.entry.ring() == RingKind::Main)
        .copied()
        .collect();
    let counts = if app.state.ui.active_tab == ActiveTab::All && app.state.ui.query.is_empty() {
        format!("{} pinned / {} recent", pinned.len(), unpinned.len())
    } else {
        format!("{} items", filtered.len())
    };

    let loading = if app.state.ui.pending_search_token.is_some() {
        "Searching..."
    } else {
        ""
    };

    let shortcuts = "Enter=paste  Esc=clear/exit  Ctrl+D=detail  Ctrl+R=refresh  Ctrl+0-9=paste  Alt+X=search kind";

    container(
        row![
            text(counts).size(11).font(app.state.theme.font()),
            text(loading).size(11).font(app.state.theme.font()),
            Space::new().width(Length::Fill),
            text(shortcuts).size(10).font(app.state.theme.mono_font()),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .style(move |theme: &iced::Theme| search_bar_style(theme, radius))
    .padding(Padding::new(6.0))
    .width(Length::Fill)
    .into()
}
