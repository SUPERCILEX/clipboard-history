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
    accent_bar_style, badge_style, card_style, danger_button_style, detail_panel_style,
    divider_style, error_banner_style, icon_button_style, pill_button_style, primary_button_style,
    search_bar_style, secondary_button_style, section_header_style, warning_banner_style,
};

// ------------------------------------------------------------------
// Top-level view
// ------------------------------------------------------------------

pub fn main_view(app: &RingboardApp) -> Element<'_, Message> {
    let is_settings = app.state.ui.active_tab == ActiveTab::Settings;
    let show_sections = app.state.ui.query.is_empty() && app.state.ui.active_tab == ActiveTab::All;
    let detail_id = app.state.ui.details_requested;
    let radius = app.state.theme.border_radius();
    let has_entries = !app.filtered_entries().is_empty();

    let mut col = if is_settings {
        column![tab_bar(app)].spacing(8)
    } else {
        column![search_bar(app), tab_bar(app)].spacing(8)
    };

    if let Some(ref e) = app.state.ui.fatal_error {
        col = col.push(fatal_error_banner(app, e, radius));
    }

    if let Some(ref e) = app.state.ui.last_error {
        col = col.push(error_banner(app, e, radius));
    }

    let content: Element<Message> = if is_settings {
        settings_view(app, radius)
    } else if !has_entries && app.state.ui.pending_search_token.is_none() {
        empty_state(app)
    } else {
        entry_list(app, detail_id)
    };
    col = col.push(content);

    if !is_settings && show_sections && has_entries {
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

pub fn search_input_id() -> iced::widget::Id {
    iced::widget::Id::new("search-input")
}

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
        .id(search_input_id())
        .on_input(Message::SearchChanged)
        .width(Length::Fill)
        .size(app.state.theme.font_size())
        .font(mono);

    let kind_button = tooltip(
        button(text(kind_label).size(12).font(mono))
            .on_press(Message::SearchKindToggled)
            .style(|theme, status| pill_button_style(theme, status, false))
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
    let font = app.state.theme.font();

    let buttons: Vec<Element<Message>> = ActiveTab::ALL
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let label = tab.label();
            let is_active = current == *tab;
            let btn = button(text(label).size(13).font(font))
                .padding(app.state.theme.button_padding())
                .style(move |theme, status| pill_button_style(theme, status, is_active))
                .on_press(Message::TabSelected(*tab));
            tooltip(
                btn,
                text(format!("Alt+{}", i + 1)).size(11),
                tooltip::Position::Bottom,
            )
            .into()
        })
        .collect();

    container(row(buttons).spacing(4).align_y(Alignment::Center))
        .padding(Padding::new(4.0))
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
            "Favorites",
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
        .id(entry_list_id())
        .height(Length::Fill)
        .into()
}

pub fn entry_list_id() -> iced::widget::Id {
    iced::widget::Id::new("entry-list")
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
            text(arrow).size(12),
            text("\u{2605}").size(13),
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

    let header = container(
        column![
            title_row,
            container(Space::new().height(Length::Fixed(1.0)))
                .width(Length::Fill)
                .style(|theme: &iced::Theme| divider_style(theme)),
        ]
        .spacing(6),
    )
    .style(move |theme: &iced::Theme| section_header_style(theme, radius))
    .padding(Padding::new(6.0).top(app.state.theme.input_padding().top))
    .width(Length::Fill);

    if collapsible {
        mouse_area(header)
            .on_press(Message::PinnedToggled)
            .into()
    } else {
        header.into()
    }
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
    let is_hovered = app.state.ui.hovered_id == Some(id);
    let is_detail_open = detail_id == Some(id);
    let radius = app.state.theme.border_radius();
    let reveal_actions = is_highlighted || is_hovered || is_detail_open;

    let preview = content_preview(app, entry, id);
    let has_detail = entry_has_extra_detail(entry);
    let actions = action_row(id, is_favorite, is_detail_open, has_detail, reveal_actions);

    let mut card_col = column![
        row![preview, Space::new().width(Length::Fill), actions]
            .spacing(8)
            .align_y(Alignment::Center),
    ]
    .spacing(6);

    if is_detail_open {
        card_col = card_col.push(detail_panel(app, entry, id, radius));
    }

    let card = container(card_col)
        .style(move |theme: &iced::Theme| card_style(theme, is_highlighted, radius))
        .padding(app.state.theme.input_padding())
        .width(Length::Fill);

    let accent = container(Space::new().width(Length::Fixed(4.0)).height(Length::Fill))
        .style(move |theme: &iced::Theme| accent_bar_style(theme, is_highlighted, radius));

    mouse_area(row![accent, card].spacing(0).align_y(Alignment::Center))
        .on_press(Message::EntryClicked(id))
        .on_enter(Message::EntryHovered(Some(id)))
        .on_exit(Message::EntryHovered(None))
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

/// Whether opening the detail panel for this entry would reveal anything
/// beyond what the row preview already shows. Used to hide the "show
/// details" toggle for entries where it would just be clutter.
pub fn entry_has_extra_detail(entry: &UiEntry) -> bool {
    match &entry.cache {
        UiEntryCache::Image => true,
        UiEntryCache::Text { one_liner } | UiEntryCache::HighlightedText { one_liner, .. } => {
            one_liner.len() > 200
        }
        UiEntryCache::Binary { .. } | UiEntryCache::Error(_) => false,
    }
}

/// The reserved width of the delete + detail icons, so revealing them on
/// hover doesn't shift the favorite star or the row's layout.
const SECONDARY_ACTIONS_WIDTH: f32 = 76.0;

fn action_row(
    id: u64,
    is_favorite: bool,
    is_detail_open: bool,
    has_detail: bool,
    reveal_secondary: bool,
) -> Element<'static, Message> {
    let star = if is_favorite { "\u{2605}" } else { "\u{2606}" };
    let detail_arrow = if is_detail_open {
        "\u{25B2}"
    } else {
        "\u{25BC}"
    };

    let favorite_button = tooltip(
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
    );

    let secondary: Element<Message> = if reveal_secondary {
        let mut items: Vec<Element<Message>> = vec![
            tooltip(
                button(text("\u{2715}").size(14))
                    .on_press(Message::DeleteEntry(id))
                    .style(danger_button_style)
                    .padding(6),
                text("Delete").size(11),
                tooltip::Position::Top,
            )
            .into(),
        ];

        if has_detail {
            items.push(
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
                )
                .into(),
            );
        }

        container(row(items).spacing(2).align_y(Alignment::Center))
            .width(Length::Fixed(SECONDARY_ACTIONS_WIDTH))
            .align_x(Alignment::End)
            .into()
    } else {
        Space::new()
            .width(Length::Fixed(SECONDARY_ACTIONS_WIDTH))
            .into()
    };

    row![favorite_button, secondary]
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
// Settings
// ------------------------------------------------------------------

fn settings_section<'a>(
    app: &'a RingboardApp,
    title: &'a str,
    body: Element<'a, Message>,
    radius: f32,
) -> Element<'a, Message> {
    container(
        column![text(title).size(14).font(app.state.theme.font()), body]
            .spacing(10)
            .width(Length::Fill),
    )
    .style(move |theme: &iced::Theme| search_bar_style(theme, radius))
    .padding(app.state.theme.input_padding())
    .width(Length::Fill)
    .into()
}

fn labeled_field<'a>(
    app: &'a RingboardApp,
    label: &'a str,
    value: &'a str,
    on_change: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        text(label).size(13).font(app.state.theme.font()).width(Length::FillPortion(2)),
        text_input("", value)
            .on_input(on_change)
            .size(app.state.theme.font_size())
            .font(app.state.theme.mono_font())
            .width(Length::FillPortion(1)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn settings_view(app: &RingboardApp, radius: f32) -> Element<'_, Message> {
    let settings = &app.state.settings;

    let server_limits = settings_section(
        app,
        "Server limits",
        column![
            labeled_field(
                app,
                "Max main entries",
                &settings.max_main_entries,
                Message::SettingsMaxMainChanged,
            ),
            labeled_field(
                app,
                "Max favorite entries",
                &settings.max_favorite_entries,
                Message::SettingsMaxFavoritesChanged,
            ),
            text("Applies after restarting the Ringboard server.")
                .size(11)
                .font(app.state.theme.font()),
            button(text(if settings.saving { "Saving..." } else { "Save" }).size(13))
                .on_press_maybe((!settings.saving).then_some(Message::SettingsSaveRequested))
                .style(primary_button_style)
                .padding(app.state.theme.button_padding()),
        ]
        .spacing(8)
        .into(),
        radius,
    );

    let maintenance = settings_section(
        app,
        "Maintenance",
        column![
            labeled_field(
                app,
                "Max wasted bytes before compacting",
                &settings.gc_max_wasted_bytes,
                Message::SettingsGcBytesChanged,
            ),
            text("0 forces a full compaction and duplicate cleanup.")
                .size(11)
                .font(app.state.theme.font()),
            button(
                text(if settings.running_gc {
                    "Running..."
                } else {
                    "Run garbage collection"
                })
                .size(13)
            )
            .on_press_maybe((!settings.running_gc).then_some(Message::SettingsGcRequested))
            .style(secondary_button_style)
            .padding(app.state.theme.button_padding()),
        ]
        .spacing(8)
        .into(),
        radius,
    );

    let mut col = column![server_limits, maintenance].spacing(12);

    if let Some(ref status) = settings.status {
        let (message, is_err) = match status {
            Ok(msg) => (msg.as_str(), false),
            Err(msg) => (msg.as_str(), true),
        };
        col = col.push(if is_err {
            error_banner_text(app, message, radius)
        } else {
            text(message).size(12).font(app.state.theme.font()).into()
        });
    }

    scrollable(col.padding(app.state.theme.input_padding()))
        .height(Length::Fill)
        .into()
}

fn error_banner_text<'a>(app: &'a RingboardApp, message: &'a str, radius: f32) -> Element<'a, Message> {
    container(text(message).size(12).font(app.state.theme.font()))
        .style(move |theme: &iced::Theme| error_banner_style(theme, radius))
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
        format!("{} favorites / {} recent", pinned.len(), unpinned.len())
    } else {
        format!("{} items", filtered.len())
    };

    let loading = if app.state.ui.pending_search_token.is_some() {
        "Searching..."
    } else {
        ""
    };

    let shortcuts = "Enter paste  \u{b7}  Esc clear/exit  \u{b7}  Ctrl+D detail  \u{b7}  Ctrl+R refresh  \u{b7}  Ctrl+0-9 paste  \u{b7}  Alt+X search kind";

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
