#![feature(let_chains)]
#![allow(clippy::significant_drop_tightening)]

use std::{
    collections::HashSet,
    env,
    error::Error,
    hash::BuildHasherDefault,
    str,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
        mpsc::{Receiver, Sender},
    },
    thread,
};

use eframe::{
    egui,
    egui::{
        CentralPanel, Event, FontId, FontTweak, Frame, Image, Key, Label, Margin, Modifiers,
        PopupCloseBehavior, Pos2, Response, RichText, ScrollArea, Sense, Stroke, TextEdit,
        TextFormat, ThemePreference, TopBottomPanel, Ui, Vec2, ViewportBuilder, ViewportCommand,
        Widget,
        text::{LayoutJob, LayoutSection},
    },
    epaint::FontFamily,
};
use itoa::Integer;
use ringboard_sdk::{
    ClientError,
    core::{Error as CoreError, protocol::RingKind},
    search::CancellationToken,
    ui_actor::{
        Command, CommandError, DetailedEntry, Message, SearchKind, UiEntry, UiEntryCache,
        controller,
    },
};
use rustc_hash::FxHasher;
use rustix::fs::unlink;

use crate::{
    loader::RingboardLoader,
    startup::{maintain_single_instance, sleep_file_name},
};

mod startup;

#[cfg(feature = "trace")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

fn main() -> Result<(), eframe::Error> {
    let stop = Arc::new(AtomicBool::new(false));
    let result = eframe::run_native(
        concat!("Ringboard v", env!("CARGO_PKG_VERSION")),
        eframe::NativeOptions {
            viewport: ViewportBuilder::default()
                .with_app_id("ringboard-egui")
                .with_min_inner_size(Vec2::splat(100.))
                .with_inner_size(Vec2::new(666., 777.))
                .with_position(Pos2::ZERO),
            ..Default::default()
        },
        Box::new(|cc| {
            let (command_sender, command_receiver) = mpsc::channel();
            let (response_sender, response_receiver) = mpsc::sync_channel(8);

            thread::spawn({
                let ctx = cc.egui_ctx.clone();
                let command_sender = command_sender.clone();
                let response_sender = response_sender.clone();
                move || {
                    {
                        let mut fonts = egui::FontDefinitions::default();

                        fonts.font_data.insert(
                            "AtkinsonHyperlegible".to_owned(),
                            egui::FontData::from_static(include_bytes!(
                                "../fonts/Atkinson-Hyperlegible-Regular-102.ttf"
                            ))
                            .tweak(FontTweak {
                                y_offset_factor: 0.1,
                                baseline_offset_factor: -0.04,
                                ..FontTweak::default()
                            })
                            .into(),
                        );
                        fonts.font_data.insert(
                            "Cascadia".to_owned(),
                            egui::FontData::from_static(include_bytes!(
                                "../fonts/CascadiaCode-Light.ttf"
                            ))
                            .into(),
                        );
                        fonts.font_data.insert(
                            "NotoEmoji".to_owned(),
                            egui::FontData::from_static(include_bytes!(
                                "../fonts/NotoEmoji-Regular.ttf"
                            ))
                            .into(),
                        );

                        fonts
                            .families
                            .entry(FontFamily::Monospace)
                            .or_default()
                            .extend_from_slice(&["Cascadia".into(), "NotoEmoji".into()]);
                        fonts
                            .families
                            .entry(FontFamily::Proportional)
                            .or_default()
                            .extend_from_slice(&[
                                "AtkinsonHyperlegible".to_owned(),
                                "NotoEmoji".to_owned(),
                            ]);

                        ctx.set_fonts(fonts);
                    }

                    let ringboard_loader = Arc::new(RingboardLoader::new(command_sender));
                    ctx.add_image_loader(ringboard_loader.clone());

                    controller(&command_receiver, |m| {
                        let r = if let Message::LoadedImage { id, image } = m {
                            ringboard_loader.add(id, image);
                            Ok(())
                        } else {
                            response_sender.send(m)
                        };
                        if r.is_ok() {
                            ctx.request_repaint();
                        }
                        r
                    });
                }
            });

            thread::spawn({
                let ctx = cc.egui_ctx.clone();
                let stop = stop.clone();
                move || {
                    ctx.send_viewport_cmd(ViewportCommand::Icon(Some(
                        eframe::icon_data::from_png_bytes(include_bytes!("../logo.jpeg"))
                            .unwrap()
                            .into(),
                    )));

                    if let Err(e) = maintain_single_instance(&stop, || {
                        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(ViewportCommand::Focus);
                    }) {
                        let _ = response_sender.send(Message::Error(e.into()));
                    }
                }
            });

            if env::var_os("THEME").as_deref().unwrap_or_default() == "light" {
                cc.egui_ctx.set_theme(ThemePreference::Light);
            }

            Ok(Box::new(App::start(command_sender, response_receiver)))
        }),
    );

    stop.store(true, Ordering::Relaxed);
    {
        let sleep_file = sleep_file_name();
        let _ = unlink(&sleep_file)
            .inspect_err(|e| eprintln!("Failed to delete sleep file: {sleep_file:?}\nError: {e}"));
    }

    result
}

struct App {
    requests: Sender<Command>,
    responses: Receiver<Message>,

    state: State,
}

#[derive(Default)]
struct State {
    entries: UiEntries,
    ui: UiState,
}

#[derive(Default)]
struct UiEntries {
    loaded_entries: Box<[UiEntry]>,
    search_results: Box<[UiEntry]>,
}

#[derive(Default)]
struct UiState {
    fatal_error: Option<ClientError>,
    last_error: Option<CommandError>,
    highlighted_id: Option<u64>,

    details_requested: Option<u64>,
    detailed_entry: Option<Result<DetailedEntry, CoreError>>,

    query: String,
    search_highlighted_id: Option<u64>,
    search_kind: SearchKind,
    pending_search_token: Option<CancellationToken>,
    queued_searches: u32,

    was_focused: bool,
    skip_first_focus: bool,

    uri_buf: UriBuf,
}

const URI_PREFIX: &str = "ringboard://";

struct UriBuf {
    buf: [u8; URI_PREFIX.len() + u64::MAX_STR_LEN],
}

impl UriBuf {
    fn format(&mut self, id: u64) -> &str {
        let mut buf = itoa::Buffer::new();
        let str = buf.format(id);

        self.buf[URI_PREFIX.len()..][..str.len()].copy_from_slice(str.as_bytes());
        unsafe { str::from_utf8_unchecked(&self.buf[..URI_PREFIX.len() + str.len()]) }
    }
}

impl Default for UriBuf {
    fn default() -> Self {
        Self {
            buf: *b"ringboard://\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        }
    }
}

impl App {
    fn start(requests: Sender<Command>, responses: Receiver<Message>) -> Self {
        let mut state = State::default();
        state.ui.skip_first_focus = true;
        Self {
            requests,
            responses,

            state,
        }
    }
}

macro_rules! active_entries {
    ($entries:expr, $state:expr) => {{
        if $state.query.is_empty() {
            &$entries.loaded_entries
        } else {
            &$entries.search_results
        }
    }};
}

macro_rules! active_highlighted_id {
    ($state:ident) => {{
        if $state.query.is_empty() {
            &mut $state.highlighted_id
        } else {
            &mut $state.search_highlighted_id
        }
    }};
}

fn remove_old_images<'a, 'b>(
    ctx: &egui::Context,
    uri_buf: &mut UriBuf,
    old: impl IntoIterator<Item = &'a UiEntry>,
    new: impl IntoIterator<Item = &'b UiEntry>,
) {
    let new_ids: HashSet<_, BuildHasherDefault<FxHasher>> = new
        .into_iter()
        .map(|UiEntry { entry, cache: _ }| entry.rai())
        .collect();

    for dead_id in old
        .into_iter()
        .map(|UiEntry { entry, cache: _ }| entry.rai())
        .filter(|id| !new_ids.contains(id))
    {
        ctx.forget_image(uri_buf.format(dead_id.id()));
    }
}

fn handle_message(message: Message, State { entries, ui }: &mut State, ctx: &egui::Context) {
    let UiEntries {
        loaded_entries,
        search_results,
    } = entries;
    let UiState {
        fatal_error,
        last_error,
        highlighted_id,
        details_requested,
        detailed_entry,
        query: _,
        search_highlighted_id,
        search_kind: _,
        pending_search_token,
        queued_searches,
        was_focused: _,
        skip_first_focus: _,
        uri_buf,
    } = ui;

    let mut remove_old_images = |entries| {
        remove_old_images(
            ctx,
            uri_buf,
            loaded_entries.iter().chain(&*search_results),
            entries,
        );
    };

    last_error.take();
    match message {
        Message::FatalDbOpen(e) => *fatal_error = Some(e.into()),
        Message::Error(e) => {
            *last_error = Some(e);
            *queued_searches = queued_searches.saturating_sub(1);
        }
        Message::LoadedFirstPage {
            entries,
            default_focused_id,
        } => {
            remove_old_images(entries.iter().chain(&*search_results));
            *loaded_entries = entries;
            if highlighted_id.is_none() {
                *highlighted_id = default_focused_id;
            }
        }
        Message::EntryDetails { id, result } => {
            if *details_requested == Some(id) {
                *detailed_entry = Some(result);
            }
        }
        Message::SearchResults(entries) => {
            remove_old_images(entries.iter().chain(&*loaded_entries));
            *queued_searches = queued_searches.saturating_sub(1);
            if pending_search_token.take().is_some() {
                *search_highlighted_id = entries.first().map(|e| e.entry.id());
                *search_results = entries;
            }
        }
        Message::FavoriteChange(id) => *active_highlighted_id!(ui) = Some(id),
        Message::Deleted(_) => {}
        Message::LoadedImage { .. } => unreachable!(),
        Message::PendingSearch(token) => {
            if *queued_searches > 1 {
                token.cancel();
            }
            *pending_search_token = Some(token);
        }
        Message::Pasted => ctx.send_viewport_cmd(ViewportCommand::Close),
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for message in self.responses.try_iter() {
            handle_message(message, &mut self.state, ctx);
        }

        let up_pressed = ctx
            .input_mut(|i| i.key_pressed(Key::ArrowUp) || i.consume_key(Modifiers::CTRL, Key::K));
        let down_pressed = ctx
            .input_mut(|i| i.key_pressed(Key::ArrowDown) || i.consume_key(Modifiers::CTRL, Key::J));

        TopBottomPanel::top("search_bar")
            .frame(Frame::side_top_panel(&ctx.style()).inner_margin(0.))
            .show(ctx, |ui| {
                search_ui(
                    ui,
                    &mut self.state,
                    &self.requests,
                    up_pressed,
                    down_pressed,
                );
            });
        CentralPanel::default()
            .frame(Frame::central_panel(&ctx.style()).inner_margin(Margin {
                top: 5.,
                ..Margin::ZERO
            }))
            .show(ctx, |ui| {
                main_ui(
                    ui,
                    &mut self.state,
                    &self.requests,
                    up_pressed,
                    down_pressed,
                );
            });

        #[cfg(not(feature = "wayland"))]
        if {
            // i3 thinks the closed window is focused if it moves monitors.
            option_env!("XDG_CURRENT_DESKTOP").is_none_or(|de| !de.eq_ignore_ascii_case("i3"))
        } && ctx.input(|i| i.viewport().close_requested() && !i.modifiers.shift)
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));

            self.state = State::default();
            ctx.forget_all_images();
        }
    }
}

fn search_ui(
    ui: &mut Ui,
    &mut State {
        entries:
            UiEntries {
                ref loaded_entries,
                ref mut search_results,
            },
        ui:
            UiState {
                ref mut query,
                ref mut search_kind,
                ref mut search_highlighted_id,
                ref mut pending_search_token,
                ref mut queued_searches,
                ref was_focused,
                ref mut uri_buf,
                ..
            },
    }: &mut State,
    requests: &Sender<Command>,
    up_pressed: bool,
    down_pressed: bool,
) {
    macro_rules! search {
        () => {
            if let Some(token) = pending_search_token {
                token.cancel();
            }
            let _ = requests.send(Command::Search {
                query: query.clone().into(),
                kind: *search_kind,
            });
            *queued_searches += 1;
        };
    }

    if ui.input_mut(|i| i.consume_key(Modifiers::ALT, Key::X)) {
        *search_kind = match search_kind {
            SearchKind::Regex => SearchKind::Plain,
            SearchKind::Plain | SearchKind::Mime => SearchKind::Regex,
        };
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Text(_))));
        search!();
    }
    if ui.input_mut(|i| i.consume_key(Modifiers::ALT, Key::M)) {
        *search_kind = match search_kind {
            SearchKind::Mime => SearchKind::Plain,
            SearchKind::Plain | SearchKind::Regex => SearchKind::Mime,
        };
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Text(_))));
        search!();
    }

    let response = ui.add(
        TextEdit::singleline(query)
            .hint_text(match search_kind {
                SearchKind::Plain => "Search",
                SearchKind::Regex => "RegEx search",
                SearchKind::Mime => "Mime type search",
            })
            .font(match search_kind {
                SearchKind::Plain => FontId::proportional(17.5),
                SearchKind::Regex | SearchKind::Mime => FontId::monospace(16.),
            })
            .desired_width(f32::INFINITY)
            .cursor_at_end(true)
            .frame(false)
            .margin(8.),
    );
    let mut reset = |query: &mut String| {
        remove_old_images(
            ui.ctx(),
            uri_buf,
            loaded_entries.iter().chain(&*search_results),
            loaded_entries,
        );

        *query = String::new();
        *search_results = Box::default();
        *search_highlighted_id = None;
    };

    if ui.input(|input| input.key_pressed(Key::Escape)) && ui.memory(|mem| !mem.any_popup_open()) {
        if query.is_empty() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        } else {
            reset(query);
        }
    }
    if up_pressed || down_pressed {
        response.surrender_focus();
    }
    if ui.input(|input| input.key_pressed(Key::Slash)) {
        ui.memory_mut(egui::Memory::close_popup);
        response.request_focus();
    }
    if !was_focused && ui.input(|i| i.focused) {
        response.request_focus();
    }

    if !response.changed() {
        return;
    }
    if query.is_empty() {
        reset(query);
        return;
    }

    search!();
}

fn show_error(ui: &mut Ui, e: &dyn Error) {
    ui.label(format!("Error: {e}"));
    ui.label(format!("Details: {e:#?}"));
}

fn main_ui(
    ui: &mut Ui,
    state_: &mut State,
    requests: &Sender<Command>,
    up_pressed: bool,
    down_pressed: bool,
) {
    let State { entries, ui: state } = state_;
    let refresh = |state: &mut UiState| {
        let _ = requests.send(Command::LoadFirstPage);
        if !state.query.is_empty() {
            if let Some(token) = &state.pending_search_token {
                token.cancel();
            }
            let _ = requests.send(Command::Search {
                query: state.query.clone().into(),
                kind: state.search_kind,
            });
            state.queued_searches += 1;
        }
    };

    {
        let focused = ui.input(|i| i.focused);
        if !state.was_focused && focused && !state.skip_first_focus {
            refresh(state);
        }
        if focused {
            state.skip_first_focus = false;
        }
        state.was_focused = focused;
    }

    if let Some(ref e) = state.fatal_error {
        show_error(ui, e);
        return;
    };
    if let Some(e) = &state.last_error {
        show_error(ui, e);
    }

    let mut try_scroll = false;

    if ui.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::R)) {
        {
            let was_focused = state.was_focused;
            *state_ = State::default();
            state_.ui.was_focused = was_focused;
        }
        ui.memory_mut(egui::Memory::close_popup);
        refresh(&mut state_.ui);
        return;
    }
    let no_popups_open = ui.memory(|mem| !mem.any_popup_open());
    if !active_entries!(entries, state).is_empty() && no_popups_open {
        handle_arrow_keys(
            active_entries!(entries, state),
            active_highlighted_id!(state),
            &mut try_scroll,
            up_pressed,
            down_pressed,
        );
    }
    if ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter))
        && let Some(id) = *active_highlighted_id!(state)
    {
        let _ = requests.send(Command::Paste(id));
    }
    if let Some(UiEntry { entry, cache: _ }) = ui
        .input_mut(|input| {
            (0..10).find(|i| {
                input.consume_key(Modifiers::CTRL, match i {
                    0 => Key::Num0,
                    1 => Key::Num1,
                    2 => Key::Num2,
                    3 => Key::Num3,
                    4 => Key::Num4,
                    5 => Key::Num5,
                    6 => Key::Num6,
                    7 => Key::Num7,
                    8 => Key::Num8,
                    9 => Key::Num9,
                    _ => unreachable!(),
                })
            })
        })
        .and_then(|idx| active_entries!(entries, state).get(idx))
    {
        let _ = requests.send(Command::Paste(entry.id()));
    }

    if active_entries!(entries, state).is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(
                RichText::new(if state.queued_searches > 0 {
                    "Loading…"
                } else {
                    "Nothing to see here…"
                })
                .heading(),
            );
        });
    }

    let try_popup =
        ui.input(|input| input.key_pressed(Key::Space)) && ui.memory(|mem| mem.focused().is_none());

    let usable_height_for_popup = ui.available_size().y - 50.;
    ScrollArea::vertical().show(ui, |ui| {
        let mut prev_was_favorites = false;
        for (i, entry) in active_entries!(entries, state).iter().enumerate() {
            let next_was_favorites = entry.entry.ring() == RingKind::Favorites;
            if prev_was_favorites && !next_was_favorites {
                ui.separator();
            }
            prev_was_favorites = next_was_favorites;

            entry_ui(
                ui,
                entry,
                entries,
                state,
                requests,
                refresh,
                try_scroll,
                try_popup,
                no_popups_open,
                usable_height_for_popup,
                i,
            );
        }
    });
}

fn entry_ui(
    ui: &mut Ui,
    entry: &UiEntry,
    entries: &UiEntries,
    state: &mut UiState,
    requests: &Sender<Command>,
    refresh: impl FnMut(&mut UiState),
    try_scroll: bool,
    try_popup: bool,
    no_popups_open: bool,
    max_popup_height: f32,
    index: usize,
) {
    macro_rules! response {
        ($w:expr) => {
            row_ui(
                ui,
                $w,
                entries,
                state,
                requests,
                refresh,
                entry,
                try_scroll,
                try_popup,
                max_popup_height,
                index,
            )
        };
    }
    let response = match &entry.cache {
        UiEntryCache::Text { one_liner } | UiEntryCache::HighlightedText { one_liner, .. } => {
            let job = LayoutJob {
                text: one_liner.to_string(),
                break_on_newline: false,
                wrap: egui::text::TextWrapping {
                    max_rows: 1,
                    break_anywhere: true,
                    ..Default::default()
                },
                sections: {
                    let format = TextFormat {
                        font_id: FontId::monospace(16.),
                        color: ui.visuals().text_color(),
                        ..Default::default()
                    };
                    if let UiEntryCache::HighlightedText {
                        one_liner: _,
                        start,
                        end,
                    } = entry.cache
                    {
                        vec![
                            LayoutSection {
                                leading_space: 0.0,
                                byte_range: 0..start,
                                format: format.clone(),
                            },
                            LayoutSection {
                                leading_space: 0.0,
                                byte_range: start..end,
                                format: TextFormat {
                                    underline: Stroke::new(1., ui.visuals().strong_text_color()),
                                    ..format.clone()
                                },
                            },
                            LayoutSection {
                                leading_space: 0.0,
                                byte_range: end..one_liner.len(),
                                format,
                            },
                        ]
                    } else {
                        vec![LayoutSection {
                            leading_space: 0.0,
                            byte_range: 0..one_liner.len(),
                            format,
                        }]
                    }
                },
                ..LayoutJob::default()
            };
            response!(Label::new(job).selectable(false))
        }
        UiEntryCache::Image => response!(
            Image::new(state.uri_buf.format(entry.entry.id()).to_owned())
                .max_height(250.)
                .max_width(ui.available_width() - 10.)
                .fit_to_original_size(1.)
        ),
        UiEntryCache::Binary { mime_type } => response!(
            Label::new(format!("Unable to display format of type {mime_type:?}."))
                .selectable(false)
        ),
        UiEntryCache::Error(e) => {
            show_error(ui, e);
            return;
        }
    };
    if response.clicked() && no_popups_open {
        let _ = requests.send(Command::Paste(entry.entry.id()));
    }
}

fn row_ui(
    ui: &mut Ui,
    widget: impl Widget,
    entries: &UiEntries,
    state: &mut UiState,
    requests: &Sender<Command>,
    mut refresh: impl FnMut(&mut UiState),
    &UiEntry { entry, ref cache }: &UiEntry,
    try_scroll: bool,
    try_popup: bool,
    max_popup_height: f32,
    index: usize,
) -> Response {
    if index < 10 && ui.input(|i| i.modifiers.ctrl) {
        egui::Area::new(ui.next_auto_id())
            .fixed_pos(ui.next_widget_position())
            .show(ui.ctx(), |ui| {
                ui.code(index.to_string());
            });
    }

    let entry_id = entry.id();

    let frame_data = Frame::default().inner_margin(5.);
    let mut frame = frame_data.begin(ui);
    frame.content_ui.add(widget);
    frame
        .content_ui
        .set_min_width(frame.content_ui.available_width());
    let response = ui.allocate_rect(
        frame.content_ui.min_rect() + (frame_data.inner_margin + frame_data.outer_margin),
        Sense::click(),
    );
    let highlighted_id = active_highlighted_id!(state);

    if try_scroll {
        if *highlighted_id == Some(entry_id) {
            response.scroll_to_me(None);
        }
    } else if response.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
        *highlighted_id = Some(entry_id);
    }
    if *highlighted_id == Some(entry_id) {
        frame.frame.fill = ui.style().visuals.widgets.hovered.weak_bg_fill;
    }
    frame.paint(ui);

    let popup_id = ui.make_persistent_id(entry_id);
    if response.secondary_clicked() || (try_popup && *highlighted_id == Some(entry_id)) {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    egui::popup::popup_below_widget(
        ui,
        popup_id,
        &response,
        PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            if state.details_requested != Some(entry_id) {
                state.details_requested = Some(entry_id);
                state.detailed_entry = None;
                let _ = requests.send(Command::GetDetails {
                    id: entry_id,
                    with_text: cache.is_text(),
                });
            }

            ui.set_max_width(frame.content_ui.available_width());
            ui.set_max_height(max_popup_height);

            ui.horizontal(|ui| {
                let mut run = |ui: &mut Ui, command| {
                    let _ = requests.send(command);
                    refresh(state);
                    ui.memory_mut(egui::Memory::close_popup);
                };

                match entry.ring() {
                    RingKind::Favorites => {
                        if ui.button("Unfavorite").clicked() {
                            run(ui, Command::Unfavorite(entry_id));
                        }
                    }
                    RingKind::Main => {
                        if ui.button("Favorite").clicked() {
                            run(ui, Command::Favorite(entry_id));
                        }
                    }
                }
                if ui.button("Delete").clicked() {
                    run(ui, Command::Delete(entry_id));

                    let entries = active_entries!(entries, state);
                    *active_highlighted_id!(state) = entries
                        .get(index.saturating_add(1))
                        .or_else(|| entries.get(index.saturating_sub(1)))
                        .map(|e| e.entry.id());
                }
            });
            ui.separator();

            ui.label(format!("Id: {entry_id}"));
            match &state.detailed_entry {
                None => {
                    ui.separator();
                    ui.label("Loading…");
                }
                Some(Ok(DetailedEntry {
                    mime_type,
                    full_text,
                })) => {
                    if !mime_type.is_empty() {
                        ui.label(format!("Mime type: {mime_type}"));
                    }
                    ui.separator();
                    if let Some(full) = full_text {
                        ScrollArea::both()
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.label(RichText::new(&**full).monospace());
                            });
                    } else if matches!(cache, UiEntryCache::Image) {
                        ScrollArea::vertical()
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.add(
                                    Image::new(state.uri_buf.format(entry.id()))
                                        .max_width(ui.available_width())
                                        .fit_to_original_size(1.),
                                );
                            });
                    } else {
                        ui.label("Binary data.");
                    }
                }
                Some(Err(e)) => {
                    ui.label(format!("Failed to get entry details:\n{e}"));
                }
            }
        },
    );
    response
}

fn handle_arrow_keys(
    entries: &[UiEntry],
    highlighted_id: &mut Option<u64>,
    try_scroll: &mut bool,
    up_pressed: bool,
    down_pressed: bool,
) {
    if up_pressed {
        *try_scroll = true;
        *highlighted_id = if let &mut Some(id) = highlighted_id {
            let idx = entries.iter().position(|e| e.entry.id() == id);
            if idx == Some(0) || idx.is_none() {
                entries.last().map(|e| e.entry.id())
            } else {
                idx.map(|idx| idx - 1)
                    .and_then(|idx| entries.get(idx))
                    .map(|e| e.entry.id())
            }
        } else {
            entries.last().map(|e| e.entry.id())
        }
    }
    if down_pressed {
        *try_scroll = true;
        *highlighted_id = if let &mut Some(id) = highlighted_id {
            let idx = entries.iter().position(|e| e.entry.id() == id);
            if idx == Some(entries.len() - 1) || idx.is_none() {
                entries.first().map(|e| e.entry.id())
            } else {
                idx.map(|idx| idx + 1)
                    .and_then(|idx| entries.get(idx))
                    .map(|e| e.entry.id())
            }
        } else {
            entries.first().map(|e| e.entry.id())
        }
    }
}

mod loader {
    use std::{
        collections::{HashMap, hash_map::Entry},
        hash::BuildHasherDefault,
        str::FromStr,
        sync::{Arc, Mutex, mpsc::Sender},
    };

    use eframe::{
        egui,
        egui::{
            ColorImage, SizeHint,
            load::{ImageLoadResult, ImageLoader, ImagePoll, LoadError},
        },
    };
    use image::DynamicImage;
    use ringboard_sdk::{core::RingAndIndex, ui_actor::Command};
    use rustc_hash::FxHasher;

    use crate::URI_PREFIX;

    enum CachedImage {
        Queued,
        Computed(Arc<ColorImage>),
    }

    pub struct RingboardLoader {
        requests: Sender<Command>,
        cache: Mutex<HashMap<RingAndIndex, CachedImage, BuildHasherDefault<FxHasher>>>,
    }

    impl RingboardLoader {
        pub const ID: &'static str = egui::generate_loader_id!(RingboardLoader);

        pub fn new(requests: Sender<Command>) -> Self {
            Self {
                requests,
                cache: Mutex::default(),
            }
        }

        pub fn add(&self, id: u64, image: DynamicImage) {
            let size = [image.width() as _, image.height() as _];
            let image_buffer = image.into_rgba8();
            let pixels = image_buffer.into_flat_samples();
            let Ok(mut cache) = self.cache.lock() else {
                return;
            };
            cache.insert(
                RingAndIndex::from_id(id).unwrap(),
                CachedImage::Computed(
                    ColorImage::from_rgba_unmultiplied(size, pixels.as_slice()).into(),
                ),
            );
        }
    }

    fn uri_to_id(uri: &str) -> Option<RingAndIndex> {
        uri.strip_prefix(URI_PREFIX)
            .and_then(|id| u64::from_str(id).ok())
            .and_then(|id| RingAndIndex::from_id(id).ok())
    }

    impl ImageLoader for RingboardLoader {
        fn id(&self) -> &str {
            Self::ID
        }

        fn load(&self, _: &egui::Context, uri: &str, _: SizeHint) -> ImageLoadResult {
            let Some(id) = uri_to_id(uri) else {
                return Err(LoadError::NotSupported);
            };

            let Ok(mut cache) = self.cache.lock() else {
                return Err(LoadError::Loading(
                    "Ringboard loader lock poisoned.".to_string(),
                ));
            };
            match cache.entry(id) {
                Entry::Occupied(e) => match e.get() {
                    CachedImage::Queued => Ok(ImagePoll::Pending { size: None }),
                    CachedImage::Computed(image) => Ok(ImagePoll::Ready {
                        image: image.clone(),
                    }),
                },
                Entry::Vacant(v) => {
                    let _ = self.requests.send(Command::LoadImage(id.id()));
                    v.insert(CachedImage::Queued);
                    Ok(ImagePoll::Pending { size: None })
                }
            }
        }

        fn forget(&self, uri: &str) {
            if let Some(id) = uri_to_id(uri)
                && let Ok(mut cache) = self.cache.lock()
            {
                cache.remove(&id);
            }
        }

        fn forget_all(&self) {
            if let Ok(mut cache) = self.cache.lock() {
                *cache = HashMap::default();
            }
        }

        fn byte_size(&self) -> usize {
            let Ok(cache) = self.cache.lock() else {
                return 0;
            };

            cache.capacity() * size_of::<CachedImage>()
                + cache
                    .values()
                    .map(|e| match e {
                        CachedImage::Queued => 0,
                        CachedImage::Computed(image) => {
                            image.pixels.capacity() * size_of::<egui::Color32>()
                        }
                    })
                    .sum::<usize>()
        }
    }
}
