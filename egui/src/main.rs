use std::str;

use eframe::{
    egui,
    egui::{text::LayoutJob, Pos2, ScrollArea, Ui, Vec2, ViewportBuilder},
};
use ringboard_sdk::{core::dirs::data_dir, DatabaseReader, Entry, EntryReader};

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
        Box::new(|_| Box::new(App::start())),
    )
}

struct App {
    database: Option<(DatabaseReader, EntryReader)>,
}

impl App {
    fn start() -> Self {
        let mut dir = data_dir();
        let database = DatabaseReader::open(&mut dir);
        let reader = EntryReader::open(&mut dir);

        Self {
            database: database.and_then(|d| Ok((d, reader?))).ok(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let Some((database, reader)) = &mut self.database else {
                ui.label(format!("Error: failed to open database: {:?}.", data_dir()));
                return;
            };

            ScrollArea::vertical().show(ui, |ui| {
                let mut show_entry = |ui: &mut Ui, entry: Entry| match entry.to_slice(reader) {
                    Ok(loaded) => {
                        if let Ok(s) = str::from_utf8(&loaded) {
                            let mut text = String::new();
                            let mut prev_char_iswhitespace = false;
                            for c in s.chars() {
                                if ['\n', '\r'].contains(&c) {
                                    continue;
                                }
                                if (prev_char_iswhitespace || text.is_empty()) && c.is_whitespace()
                                {
                                    continue;
                                }

                                text.push(c);
                                prev_char_iswhitespace = c.is_whitespace();
                            }

                            let mut job =
                                LayoutJob::single_section(text, egui::TextFormat::default());
                            job.wrap = egui::text::TextWrapping {
                                max_rows: 1,
                                break_anywhere: true,
                                ..Default::default()
                            };
                            ui.label(job).on_hover_text(s);
                        } else {
                            ui.label("TODO non-text entry.");
                        }
                    }
                    Err(e) => {
                        ui.label(format!("Error: failed to load entry {entry:?}: {e:?}"));
                    }
                };

                database.favorites().rev().for_each(|e| show_entry(ui, e));
                ui.separator();
                database
                    .main()
                    .rev()
                    .take(100)
                    .for_each(|e| show_entry(ui, e));
            });
        });
    }
}
