use std::{
    iter::once,
    str,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender, SyncSender},
        Arc,
    },
    thread,
};

use eframe::{
    egui,
    egui::{
        text::LayoutJob, Align, CentralPanel, FontId, Key, Label, Layout, Pos2, ScrollArea, Sense,
        TextFormat, Ui, Vec2, ViewportBuilder,
    },
    epaint::FontFamily,
};
use ringboard_sdk::{
    core::{dirs::data_dir, protocol::RingKind, Error as CoreError},
    DatabaseReader, Entry, EntryReader,
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
                || controller(ctx, command_receiver, response_sender)
            });
            Box::new(App::start(cascadia, command_sender, response_receiver))
        }),
    )
}

enum Command {
    LoadFirstPage,
}

enum Message {
    FatalDbOpen(CoreError),
    LoadedFirstPage {
        entries: Vec<UiEntry>,
        first_non_favorite_id: Option<u64>,
    },
}

fn controller(ctx: egui::Context, commands: Receiver<Command>, responses: SyncSender<Message>) {
    let (database, mut reader) = {
        let mut dir = data_dir();
        let database = DatabaseReader::open(&mut dir);
        let reader = EntryReader::open(&mut dir);
        match database.and_then(|d| Ok((d, reader?))) {
            Ok(db) => db,
            Err(e) => {
                let _ = responses.send(Message::FatalDbOpen(e));
                ctx.request_repaint();
                return;
            }
        }
    };

    for command in once(Command::LoadFirstPage).chain(commands) {
        if responses
            .send(handle_command(command, &database, &mut reader))
            .is_err()
        {
            break;
        }
        ctx.request_repaint();
    }
}

fn handle_command(
    command: Command,
    database: &DatabaseReader,
    reader: &mut EntryReader,
) -> Message {
    match command {
        Command::LoadFirstPage => {
            let mut entries = Vec::with_capacity(100);
            for entry in database
                .favorites()
                .rev()
                .chain(database.main().rev().take(100))
            {
                entries.push(ui_entry(entry, reader).unwrap_or_else(|e| UiEntry {
                    cache: UiEntryCache::Error(format!(
                        "Error: failed to load entry {entry:?}\n{e:?}"
                    )),
                    entry,
                    mime_type: String::new(),
                }));
            }
            Message::LoadedFirstPage {
                entries,
                first_non_favorite_id: database.main().rev().nth(1).as_ref().map(Entry::id),
            }
        }
    }
}

struct UiEntry {
    entry: Entry,
    mime_type: String,
    cache: UiEntryCache,
}

#[derive(Clone)]
enum UiEntryCache {
    Text { one_liner: String, full: String },
    Binary { bytes: Arc<[u8]> },
    Error(String),
}

struct App {
    requests: Sender<Command>,
    responses: Receiver<Message>,
    row_font: FontFamily,

    state: UiState,
}

#[derive(Default)]
struct UiState {
    fatal_error: Option<CoreError>,
    loaded_entries: Vec<UiEntry>,
    highlighted_id: Option<u64>,
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

            state: UiState::default(),
        }
    }
}

fn ui_entry(entry: Entry, reader: &mut EntryReader) -> Result<UiEntry, CoreError> {
    let loaded = entry.to_slice(reader)?;
    let mime_type = String::from(&*loaded.mime_type()?);
    let entry = if let Ok(s) = str::from_utf8(&loaded) {
        let mut one_liner = String::new();
        let mut prev_char_is_whitespace = false;
        for c in s.chars() {
            if (prev_char_is_whitespace || one_liner.is_empty()) && c.is_whitespace() {
                continue;
            }

            one_liner.push(if c.is_whitespace() { ' ' } else { c });
            prev_char_is_whitespace = c.is_whitespace();
        }

        UiEntry {
            entry,
            mime_type,
            cache: UiEntryCache::Text {
                one_liner,
                full: s.to_string(),
            },
        }
    } else {
        UiEntry {
            entry,
            mime_type,
            cache: UiEntryCache::Binary {
                bytes: loaded.into_inner().into_owned().into(),
            },
        }
    };
    Ok(entry)
}

fn handle_message(message: Message, state: &mut UiState) {
    match message {
        Message::FatalDbOpen(e) => state.fatal_error = Some(e),
        Message::LoadedFirstPage {
            entries,
            first_non_favorite_id,
        } => {
            state.loaded_entries = entries;
            state.highlighted_id = first_non_favorite_id;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for message in self.responses.try_iter() {
            handle_message(message, &mut self.state);
        }

        CentralPanel::default().show(ctx, |ui| {
            main_ui(ui, self.row_font.clone(), &mut self.state, &self.requests)
        });
    }
}

fn main_ui(
    ui: &mut Ui,
    entry_text_font: FontFamily,
    state: &mut UiState,
    requests: &Sender<Command>,
) {
    if let Some(ref e) = state.fatal_error {
        ui.label(format!("Fatal error: {e:?}"));
        return;
    };

    let mut try_scroll = false;
    if !state.loaded_entries.is_empty() {
        ui.input(|input| {
            if input.key_pressed(Key::ArrowUp) {
                try_scroll = true;
                if let Some(id) = state.highlighted_id {
                    let idx = state.loaded_entries.iter().position(|e| e.entry.id() == id);
                    if idx == Some(0) || idx.is_none() {
                        state.highlighted_id = state.loaded_entries.last().map(|e| e.entry.id());
                    } else {
                        state.highlighted_id = idx
                            .map(|idx| idx - 1)
                            .and_then(|idx| state.loaded_entries.get(idx))
                            .map(|e| e.entry.id());
                    }
                } else {
                    state.highlighted_id = state.loaded_entries.last().map(|e| e.entry.id());
                }
            }
            if input.key_pressed(Key::ArrowDown) {
                try_scroll = true;
                if let Some(id) = state.highlighted_id {
                    let idx = state.loaded_entries.iter().position(|e| e.entry.id() == id);
                    if idx == Some(state.loaded_entries.len() - 1) || idx.is_none() {
                        state.highlighted_id = state.loaded_entries.first().map(|e| e.entry.id());
                    } else {
                        state.highlighted_id = idx
                            .map(|idx| idx + 1)
                            .and_then(|idx| state.loaded_entries.get(idx))
                            .map(|e| e.entry.id());
                    }
                } else {
                    state.highlighted_id = state.loaded_entries.first().map(|e| e.entry.id());
                }
            }
        });
    }

    let mut try_popup = false;
    ui.input(|input| {
        if input.key_pressed(Key::Space) {
            try_popup = true;
        }
    });

    // TODO add search
    // TODO implement paste (by pressing enter or ctrl+N)
    // TODO tab cycles between selecting main or favorites
    ScrollArea::vertical().show(ui, |ui| {
        let mut show_entry = |ui: &mut Ui, entry: &UiEntry| {
            match entry.cache.clone() {
                UiEntryCache::Text { one_liner, full } => {
                    let mut job = LayoutJob::single_section(
                        one_liner,
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
                    let frame_data = egui::Frame::default().inner_margin(5.);
                    let mut frame = frame_data.begin(ui);
                    frame.content_ui.add(Label::new(job).selectable(false));
                    frame.content_ui.set_min_width(
                        frame.content_ui.available_width() - frame_data.inner_margin.right,
                    );
                    let response = ui.allocate_rect(
                        (frame_data.inner_margin + frame_data.outer_margin)
                            .expand_rect(frame.content_ui.min_rect()),
                        Sense::click(),
                    );
                    if try_scroll {
                        if state.highlighted_id == Some(entry.entry.id()) {
                            response.scroll_to_me(None);
                        }
                    } else if response.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                        state.highlighted_id = Some(entry.entry.id());
                    }
                    if state.highlighted_id == Some(entry.entry.id()) {
                        frame.frame.fill = ui
                            .style()
                            .visuals
                            .widgets
                            .hovered
                            .bg_fill
                            .linear_multiply(0.1);
                    }
                    frame.paint(ui);

                    let popup_id = ui.make_persistent_id(entry.entry.id());
                    if response.secondary_clicked()
                        || (try_popup && state.highlighted_id == Some(entry.entry.id()))
                    {
                        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
                    }
                    // TODO disappears on click for some reason
                    egui::popup::popup_below_widget(ui, popup_id, &response, |ui| {
                        ui.set_min_width(200.);

                        ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                            ui.horizontal(|ui| {
                                if ui.button("Favorite").clicked() {
                                    // TODO
                                }
                                if ui.button("Delete").clicked() {
                                    // TODO
                                }
                            });
                            ui.separator();

                            ui.label(format!("Id: {}", entry.entry.id()));
                            if !entry.mime_type.is_empty() {
                                ui.label(format!("Mime type: {}", entry.mime_type));
                            }
                            ui.separator();
                            ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                                ui.label(full);
                            });
                        });
                    });

                    if response.clicked() {
                        // TODO
                    }
                }
                UiEntryCache::Binary { bytes } => {
                    // TODO
                }
                UiEntryCache::Error(e) => {
                    ui.label(e);
                }
            }
        };

        let mut prev_was_favorites = false;
        for entry in &state.loaded_entries {
            let next_was_favorites = entry.entry.ring() == RingKind::Favorites;
            if prev_was_favorites && !next_was_favorites {
                ui.separator();
            }
            prev_was_favorites = next_was_favorites;

            show_entry(ui, entry);
        }
        // TODO add a load more button
    });
}
