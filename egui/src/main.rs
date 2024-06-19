use std::{str, sync::Arc};

use eframe::{
    egui,
    egui::{
        text::LayoutJob, Align, CentralPanel, FontId, Key, Label, Layout, Pos2, ScrollArea, Sense,
        Ui, Vec2, ViewportBuilder,
    },
    epaint::FontFamily,
};
use ringboard_sdk::{
    core::{dirs::data_dir, protocol::RingKind},
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

            Box::new(App::start(cascadia))
        }),
    )
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
    database: Option<(DatabaseReader, EntryReader)>,
    row_font: FontFamily,

    loaded_entries: Vec<UiEntry>,
    highlighted_id: Option<u64>,
}

impl App {
    fn start(row_font: FontFamily) -> Self {
        let mut dir = data_dir();
        let database = DatabaseReader::open(&mut dir);
        let reader = EntryReader::open(&mut dir);

        Self {
            database: database.and_then(|d| Ok((d, reader?))).ok(),
            row_font,

            loaded_entries: Vec::new(),
            highlighted_id: None,
        }
    }
}

fn ui_entry(entry: Entry, reader: &mut EntryReader) -> UiEntry {
    match entry.to_slice(reader) {
        Ok(loaded) => {
            let mime_type = loaded
                .mime_type()
                .ok()
                .as_deref()
                .map_or(String::new(), String::from);
            if let Ok(s) = str::from_utf8(&loaded) {
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
            }
        }
        Err(e) => UiEntry {
            cache: UiEntryCache::Error(format!("Error: failed to load entry {entry:?}\n{e:?}")),
            entry,
            mime_type: String::new(),
        },
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        CentralPanel::default().show(ctx, |ui| {
            let Some((database, reader)) = &mut self.database else {
                ui.label(format!("Error: failed to open database: {:?}.", data_dir()));
                return;
            };

            if self.loaded_entries.is_empty() {
                for entry in database
                    .favorites()
                    .rev()
                    .chain(database.main().rev().take(100))
                {
                    self.loaded_entries.push(ui_entry(entry, reader));
                }
                self.highlighted_id = database.main().rev().nth(1).as_ref().map(Entry::id);
            }

            let mut try_scroll = false;
            if !self.loaded_entries.is_empty() {
                ui.input(|input| {
                    if input.key_pressed(Key::ArrowUp) {
                        try_scroll = true;
                        if let Some(id) = self.highlighted_id {
                            let idx = self.loaded_entries.iter().position(|e| e.entry.id() == id);
                            if idx == Some(0) || idx.is_none() {
                                self.highlighted_id =
                                    self.loaded_entries.last().map(|e| e.entry.id());
                            } else {
                                self.highlighted_id = idx
                                    .map(|idx| idx - 1)
                                    .and_then(|idx| self.loaded_entries.get(idx))
                                    .map(|e| e.entry.id());
                            }
                        } else {
                            self.highlighted_id = self.loaded_entries.last().map(|e| e.entry.id());
                        }
                    }
                    if input.key_pressed(Key::ArrowDown) {
                        try_scroll = true;
                        if let Some(id) = self.highlighted_id {
                            let idx = self.loaded_entries.iter().position(|e| e.entry.id() == id);
                            if idx == Some(self.loaded_entries.len() - 1) || idx.is_none() {
                                self.highlighted_id =
                                    self.loaded_entries.first().map(|e| e.entry.id());
                            } else {
                                self.highlighted_id = idx
                                    .map(|idx| idx + 1)
                                    .and_then(|idx| self.loaded_entries.get(idx))
                                    .map(|e| e.entry.id());
                            }
                        } else {
                            self.highlighted_id = self.loaded_entries.first().map(|e| e.entry.id());
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
            // TODO start with second row of main selected
            // TODO tab cycles between selecting main or favorites
            ScrollArea::vertical().show(ui, |ui| {
                let mut show_entry = |ui: &mut Ui, entry: &UiEntry| {
                    match entry.cache.clone() {
                        UiEntryCache::Text { one_liner, full } => {
                            let mut job = LayoutJob::single_section(
                                one_liner,
                                egui::TextFormat {
                                    font_id: FontId::new(16., self.row_font.clone()),
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
                                if self.highlighted_id == Some(entry.entry.id()) {
                                    response.scroll_to_me(None);
                                }
                            } else if response.hovered()
                                && ui.input(|i| i.pointer.delta() != Vec2::ZERO)
                            {
                                self.highlighted_id = Some(entry.entry.id());
                            }
                            if self.highlighted_id == Some(entry.entry.id()) {
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
                                || (try_popup && self.highlighted_id == Some(entry.entry.id()))
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
                for entry in &self.loaded_entries {
                    let next_was_favorites = entry.entry.ring() == RingKind::Favorites;
                    if prev_was_favorites && !next_was_favorites {
                        ui.separator();
                    }
                    prev_was_favorites = next_was_favorites;

                    show_entry(ui, entry);
                }
                // TODO add a load more button
            });
        });
    }
}
