use std::{
    mem,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender},
    },
    thread,
};

use eframe::{
    egui,
    egui::{
        text::LayoutJob, Align, CentralPanel, Event, FontId, FontSelection, Image, InputState, Key,
        Label, Layout, Modifiers, PopupCloseBehavior, Pos2, Response, ScrollArea, Sense, TextEdit,
        TextFormat, TextStyle, TopBottomPanel, Ui, Vec2, ViewportBuilder, Widget,
    },
    epaint::FontFamily,
};
use ringboard_sdk::{
    core::{protocol::RingKind, Error as CoreError},
    ui_actor::{controller, Command, CommandError, DetailedEntry, Message, UiEntry, UiEntryCache},
    ClientError,
};

fn main() -> Result<(), eframe::Error> {
    eframe::run_native(
        "Ringboard",
        eframe::NativeOptions {
            viewport: ViewportBuilder::default()
                .with_min_inner_size(Vec2::splat(100.))
                .with_inner_size(Vec2::new(666., 777.))
                .with_position(Pos2::ZERO),
            ..Default::default()
        },
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);

            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cascadia".to_owned(),
                egui::FontData::from_static(include_bytes!("../CascadiaCode-Light.ttf")),
            );
            let cascadia = FontFamily::Name("cascadia".into());
            fonts
                .families
                .entry(cascadia.clone())
                .or_default()
                .push("cascadia".to_string());
            cc.egui_ctx.set_fonts(fonts);

            let (command_sender, command_receiver) = mpsc::channel();
            let (response_sender, response_receiver) = mpsc::sync_channel(8);
            thread::spawn({
                let ctx = cc.egui_ctx.clone();
                move || {
                    controller(&command_receiver, |m| {
                        let r = response_sender.send(m);
                        if r.is_ok() {
                            ctx.request_repaint();
                        }
                        r
                    });
                }
            });
            Ok(Box::new(App::start(
                cascadia,
                command_sender,
                response_receiver,
            )))
        }),
    )
}

struct App {
    requests: Sender<Command>,
    responses: Receiver<Message>,
    row_font: FontFamily,

    state: UiState,
    entries: UiEntries,
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
    search_with_regex: bool,

    was_focused: bool,
    skipped_first_focus: bool,
}

impl App {
    fn start(
        row_font: FontFamily,
        requests: Sender<Command>,
        responses: Receiver<Message>,
    ) -> Self {
        Self {
            requests,
            responses,
            row_font,

            entries: UiEntries::default(),
            state: UiState::default(),
        }
    }
}

fn handle_message(
    message: Message,
    UiEntries {
        loaded_entries,
        search_results,
    }: &mut UiEntries,
    UiState {
        fatal_error,
        last_error,
        highlighted_id,
        details_requested,
        detailed_entry,
        query: _,
        search_highlighted_id,
        search_with_regex: _,
        was_focused: _,
        skipped_first_focus: _,
    }: &mut UiState,
) {
    match message {
        Message::FatalDbOpen(e) => *fatal_error = Some(e.into()),
        Message::FatalServerConnect(e) => *fatal_error = Some(e),
        Message::Error(e) => *last_error = Some(e),
        Message::LoadedFirstPage {
            entries,
            first_non_favorite_id,
        } => {
            *loaded_entries = entries;
            if highlighted_id.is_none() {
                *highlighted_id = first_non_favorite_id;
            }
        }
        Message::EntryDetails { id, result } => {
            if *details_requested == Some(id) {
                *detailed_entry = Some(result);
            }
        }
        Message::SearchResults(entries) => {
            *search_highlighted_id = entries.first().map(|e| e.entry.id());
            *search_results = entries;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for message in self.responses.try_iter() {
            handle_message(message, &mut self.entries, &mut self.state);
        }

        TopBottomPanel::top("search_bar").show(ctx, |ui| {
            search_ui(ui, &mut self.entries, &mut self.state, &self.requests);
        });
        CentralPanel::default().show(ctx, |ui| {
            main_ui(
                ui,
                &self.row_font,
                &self.entries,
                &mut self.state,
                &self.requests,
            );
        });
    }
}

fn search_ui(
    ui: &mut Ui,
    UiEntries {
        loaded_entries: _,
        search_results,
    }: &mut UiEntries,
    UiState {
        query,
        search_with_regex,
        search_highlighted_id,
        ..
    }: &mut UiState,
    requests: &Sender<Command>,
) {
    if ui.input_mut(|i| i.consume_key(Modifiers::ALT, Key::X)) {
        *search_with_regex ^= true;
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Text(_))));
    }

    let response = ui.add(
        TextEdit::singleline(query)
            .hint_text(if *search_with_regex {
                "Search with RegEx"
            } else {
                "Search"
            })
            .font(if *search_with_regex {
                TextStyle::Monospace.into()
            } else {
                FontSelection::default()
            })
            .desired_width(f32::INFINITY)
            .cursor_at_end(true)
            .frame(false)
            .margin(5.),
    );
    let mut reset = |query: &mut String| {
        *query = String::new();
        *search_results = Box::default();
        *search_highlighted_id = None;
    };

    if ui.input(|input| input.key_pressed(Key::Escape)) && ui.memory(|mem| !mem.any_popup_open()) {
        reset(query);
    }
    if ui.input(|i| i.key_pressed(Key::ArrowUp) || i.key_pressed(Key::ArrowDown)) {
        response.surrender_focus();
    }
    if ui.input(|input| input.key_pressed(Key::Slash)) {
        ui.memory_mut(egui::Memory::close_popup);
        response.request_focus();
    }

    if !response.changed() {
        return;
    }
    if query.is_empty() {
        reset(query);
        return;
    }

    let _ = requests.send(Command::Search {
        query: query.clone().into(),
        regex: *search_with_regex,
    });
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

#[allow(clippy::too_many_lines)]
fn main_ui(
    ui: &mut Ui,
    entry_text_font: &FontFamily,
    entries: &UiEntries,
    state: &mut UiState,
    requests: &Sender<Command>,
) {
    let refresh = || {
        let _ = requests
            .send(Command::RefreshDb)
            .and_then(|()| requests.send(Command::LoadFirstPage));
    };

    ui.input(|i| {
        if !state.was_focused && i.focused && state.skipped_first_focus {
            refresh();
        }
        if i.focused {
            state.skipped_first_focus = true;
        }
        state.was_focused = i.focused;
    });

    if let Some(ref e) = state.fatal_error {
        ui.label(format!("Fatal error: {e:?}"));
        return;
    };
    if let Some(e) = mem::take(&mut state.last_error) {
        ui.label(format!("Error: {e:?}"));
    }

    let mut try_scroll = false;

    if ui.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::R)) {
        *state = UiState::default();
        ui.memory_mut(egui::Memory::close_popup);
        refresh();
    }
    if !active_entries(entries, state).is_empty() && ui.memory(|mem| !mem.any_popup_open()) {
        ui.input(|input| {
            handle_arrow_keys(
                active_entries(entries, state),
                active_highlighted_id!(state),
                &mut try_scroll,
                input,
            );
        });
    }

    let try_popup =
        ui.input(|input| input.key_pressed(Key::Space)) && ui.memory(|mem| mem.focused().is_none());

    // TODO implement paste (by pressing enter or ctrl+N)
    ScrollArea::vertical().show(ui, |ui| {
        let mut prev_was_favorites = false;
        for entry in active_entries(entries, state) {
            let next_was_favorites = entry.entry.ring() == RingKind::Favorites;
            if prev_was_favorites && !next_was_favorites {
                ui.separator();
            }
            prev_was_favorites = next_was_favorites;

            entry_ui(
                ui,
                entry_text_font,
                entry,
                state,
                requests,
                refresh,
                try_scroll,
                try_popup,
            );
        }
    });
}

fn active_entries<'a>(entries: &'a UiEntries, state: &UiState) -> &'a [UiEntry] {
    if state.query.is_empty() {
        &entries.loaded_entries
    } else {
        &entries.search_results
    }
}

#[allow(clippy::too_many_arguments)]
fn entry_ui(
    ui: &mut Ui,
    entry_text_font: &FontFamily,
    entry: &UiEntry,
    state: &mut UiState,
    requests: &Sender<Command>,
    refresh: impl FnMut(),
    try_scroll: bool,
    try_popup: bool,
) {
    let response = match entry.cache.clone() {
        UiEntryCache::Text { one_liner } => {
            let mut job = LayoutJob::single_section(
                one_liner.to_string(),
                TextFormat {
                    font_id: FontId::new(16., entry_text_font.clone()),
                    ..Default::default()
                },
            );
            job.wrap = egui::text::TextWrapping {
                max_rows: 1,
                break_anywhere: true,
                ..Default::default()
            };
            row_ui(
                ui,
                Label::new(job).selectable(false),
                state,
                requests,
                refresh,
                entry,
                try_scroll,
                try_popup,
            )
        }
        UiEntryCache::Image { uri } => row_ui(
            ui,
            Image::new(&*uri)
                .max_height(250.)
                .max_width(ui.available_width())
                .fit_to_original_size(1.),
            state,
            requests,
            refresh,
            entry,
            try_scroll,
            try_popup,
        ),
        UiEntryCache::Binary { mime_type, context } => row_ui(
            ui,
            Label::new(format!(
                "Unable to display format of type {mime_type:?} from {context:?}."
            ))
            .selectable(false),
            state,
            requests,
            refresh,
            entry,
            try_scroll,
            try_popup,
        ),
        UiEntryCache::Error(e) => {
            ui.label(&*e);
            return;
        }
    };
    if response.clicked() {
        // TODO
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn row_ui(
    ui: &mut Ui,
    widget: impl Widget,
    state: &mut UiState,
    requests: &Sender<Command>,
    mut refresh: impl FnMut(),
    &UiEntry { entry, ref cache }: &UiEntry,
    try_scroll: bool,
    try_popup: bool,
) -> Response {
    let entry_id = entry.id();

    let frame_data = egui::Frame::default().inner_margin(5.);
    let mut frame = frame_data.begin(ui);
    frame.content_ui.add(widget);
    frame
        .content_ui
        .set_min_width(frame.content_ui.available_width() - frame_data.inner_margin.right);
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
        frame.frame.fill = ui
            .style()
            .visuals
            .widgets
            .hovered
            .bg_fill
            .linear_multiply(0.1);
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
                    entry,
                    with_text: matches!(cache, UiEntryCache::Text { .. }),
                });
            }

            ui.set_min_width(200.);

            ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                ui.horizontal(|ui| {
                    match entry.ring() {
                        RingKind::Favorites => {
                            if ui.button("Unfavorite").clicked() {
                                let _ = requests.send(Command::Unfavorite(entry_id));
                                refresh();
                            }
                        }
                        RingKind::Main => {
                            if ui.button("Favorite").clicked() {
                                let _ = requests.send(Command::Favorite(entry_id));
                                refresh();
                            }
                        }
                    }
                    if ui.button("Delete").clicked() {
                        let _ = requests.send(Command::Delete(entry_id));
                        refresh();
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
                                    ui.label(&**full);
                                });
                        } else if let UiEntryCache::Image { uri } = cache {
                            ScrollArea::vertical()
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    ui.add(
                                        Image::new(&**uri)
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
            });
        },
    );
    response
}

fn handle_arrow_keys(
    entries: &[UiEntry],
    highlighted_id: &mut Option<u64>,
    try_scroll: &mut bool,
    input: &InputState,
) {
    if input.key_pressed(Key::ArrowUp) {
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
    if input.key_pressed(Key::ArrowDown) {
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
