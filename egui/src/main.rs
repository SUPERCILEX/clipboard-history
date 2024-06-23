use std::{
    iter::once,
    mem,
    os::fd::{AsFd, BorrowedFd},
    str,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender, SyncSender},
    },
    thread,
};

use eframe::{
    egui,
    egui::{
        text::LayoutJob, Align, CentralPanel, FontId, Image, Key, Label, Layout, Pos2, ScrollArea,
        Sense, TextFormat, Ui, Vec2, ViewportBuilder,
    },
    epaint::FontFamily,
};
use ringboard_sdk::{
    connect_to_server,
    core::{
        direct_file_name,
        dirs::{data_dir, socket_file},
        protocol::{MoveToFrontResponse, RemoveResponse, RingKind},
        ring::{offset_to_entries, Ring},
        Error as CoreError, IoErr, PathView,
    },
    ClientError, DatabaseReader, Entry, EntryReader,
};
use rustix::{
    fs::{openat, statx, AtFlags, Mode, OFlags, StatxFlags, CWD},
    net::SocketAddrUnix,
    process::fchdir,
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
                || controller(ctx, command_receiver, response_sender)
            });
            Box::new(App::start(cascadia, command_sender, response_receiver))
        }),
    )
}

#[derive(Debug)]
enum Command {
    RefreshDb,
    LoadFirstPage,
    GetDetails { entry: Entry, with_text: bool },
    Favorite(u64),
    Unfavorite(u64),
    Delete(u64),
}

#[derive(Debug)]
enum Message {
    FatalDbOpen(CoreError),
    FatalServerConnect(ClientError),
    Error(ClientError),
    LoadedFirstPage {
        entries: Vec<UiEntry>,
        first_non_favorite_id: Option<u64>,
    },
    EntryDetails(Result<DetailedEntry, CoreError>),
}

fn controller(ctx: egui::Context, commands: Receiver<Command>, responses: SyncSender<Message>) {
    let server = {
        match {
            let socket_file = socket_file();
            SocketAddrUnix::new(&socket_file)
                .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))
        }
        .map_err(ClientError::from)
        .and_then(|server_addr| Ok((connect_to_server(&server_addr)?, server_addr)))
        {
            Ok(server) => server,
            Err(e) => {
                let _ = responses.send(Message::FatalServerConnect(e));
                ctx.request_repaint();
                return;
            }
        }
    };
    let ((mut database, mut reader), rings) = {
        let run = || {
            let mut dir = data_dir();

            let database = DatabaseReader::open(&mut dir)?;
            let reader = EntryReader::open(&mut dir)?;

            let mut open_ring = |kind: RingKind| {
                let path = PathView::new(&mut dir, kind.file_name());
                openat(CWD, &*path, OFlags::PATH, Mode::empty()).map_io_err(|| {
                    format!("Failed to open Ringboard database for reading: {path:?}")
                })
            };
            let rings = (open_ring(RingKind::Main)?, open_ring(RingKind::Favorites)?);

            fchdir(reader.direct())
                .map_io_err(|| "Failed to change working directory to direct allocations.")?;

            Ok(((database, reader), rings))
        };

        match run() {
            Ok(db) => db,
            Err(e) => {
                let _ = responses.send(Message::FatalDbOpen(e));
                ctx.request_repaint();
                return;
            }
        }
    };

    for command in once(Command::LoadFirstPage).chain(commands) {
        let result = handle_command(
            command,
            (&server.0, &server.1),
            &mut database,
            &mut reader,
            (&rings.0, &rings.1),
        )
        .unwrap_or_else(|e| Some(Message::Error(e)));

        let Some(response) = result else {
            continue;
        };
        if responses.send(response).is_err() {
            break;
        }
        ctx.request_repaint();
    }
}

fn handle_command(
    command: Command,
    server: (impl AsFd, &SocketAddrUnix),
    database: &mut DatabaseReader,
    reader: &mut EntryReader,
    rings: (impl AsFd, impl AsFd),
) -> Result<Option<Message>, ClientError> {
    match command {
        Command::RefreshDb => {
            let run = |ring: &mut Ring, fd: BorrowedFd| {
                let len = statx(fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                    .map_io_err(|| "Failed to statx Ringboard database file.")?
                    .stx_size;
                let len = offset_to_entries(usize::try_from(len).unwrap());
                unsafe {
                    ring.set_len(len);
                }
                Ok::<_, CoreError>(())
            };
            run(database.main_ring_mut(), rings.0.as_fd())?;
            run(database.favorites_ring_mut(), rings.1.as_fd())?;

            Ok(None)
        }
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
                }));
            }
            Ok(Some(Message::LoadedFirstPage {
                entries,
                first_non_favorite_id: database.main().rev().nth(1).as_ref().map(Entry::id),
            }))
        }
        Command::GetDetails { entry, with_text } => {
            let mut run = || {
                if with_text {
                    let loaded = entry.to_slice(reader)?;
                    Ok(DetailedEntry {
                        mime_type: (&*loaded.mime_type()?).into(),
                        full_text: String::from_utf8(loaded.into_inner().into_owned()).ok(),
                    })
                } else {
                    Ok(DetailedEntry {
                        mime_type: (&*entry.to_file(reader)?.mime_type()?).into(),
                        full_text: None,
                    })
                }
            };
            Ok(Some(Message::EntryDetails(run())))
        }
        ref c @ (Command::Favorite(id) | Command::Unfavorite(id)) => {
            match ringboard_sdk::move_to_front(
                server.0,
                server.1,
                id,
                Some(match c {
                    Command::Favorite(_) => RingKind::Favorites,
                    Command::Unfavorite(_) => RingKind::Main,
                    _ => unreachable!(),
                }),
            )? {
                MoveToFrontResponse::Success { .. } => {}
                MoveToFrontResponse::Error(e) => return Err(e.into()),
            }
            Ok(None)
        }
        Command::Delete(id) => {
            match ringboard_sdk::remove(server.0, server.1, id)? {
                RemoveResponse { error: Some(e) } => return Err(e.into()),
                RemoveResponse { error: None } => {}
            }
            Ok(None)
        }
    }
}

#[derive(Debug)]
struct UiEntry {
    entry: Entry,
    cache: UiEntryCache,
}

#[derive(Clone, Debug)]
enum UiEntryCache {
    Text { one_liner: String },
    Image { uri: String },
    Binary { mime_type: String, context: String },
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
    fatal_error: Option<ClientError>,
    last_error: Option<ClientError>,
    loaded_entries: Vec<UiEntry>,
    highlighted_id: Option<u64>,

    details_requested: Option<u64>,
    detailed_entry: Option<Result<DetailedEntry, CoreError>>,
}

#[derive(Debug)]
struct DetailedEntry {
    mime_type: String,
    full_text: Option<String>,
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
    let mime_type = &*loaded.mime_type()?;
    let entry = if mime_type.starts_with("image/") {
        let mut buf = Default::default();
        let buf = direct_file_name(&mut buf, entry.ring(), entry.index());
        UiEntry {
            entry,
            cache: UiEntryCache::Image {
                uri: format!("file://{}", buf.to_str().unwrap()),
            },
        }
    } else if let Ok(s) = str::from_utf8(&loaded) {
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
            cache: UiEntryCache::Text { one_liner },
        }
    } else {
        UiEntry {
            entry,
            cache: UiEntryCache::Binary {
                mime_type: mime_type.into(),
                context: String::new(),
            },
        }
    };
    Ok(entry)
}

fn handle_message(message: Message, state: &mut UiState) {
    match message {
        Message::FatalDbOpen(e) => state.fatal_error = Some(e.into()),
        Message::FatalServerConnect(e) => state.fatal_error = Some(e),
        Message::Error(e) => state.last_error = Some(e),
        Message::LoadedFirstPage {
            entries,
            first_non_favorite_id,
        } => {
            state.loaded_entries = entries;
            if state.highlighted_id.is_none() {
                state.highlighted_id = first_non_favorite_id;
            }
        }
        Message::EntryDetails(r) => state.detailed_entry = Some(r),
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
    let refresh = || {
        let _ = requests
            .send(Command::RefreshDb)
            .and_then(|()| requests.send(Command::LoadFirstPage));
    };

    if let Some(ref e) = state.fatal_error {
        ui.label(format!("Fatal error: {e:?}"));
        return;
    };
    if let Some(e) = mem::take(&mut state.last_error) {
        ui.label(format!("Error: {e:?}"));
    }

    let mut try_scroll = false;
    ui.input(|input| {
        if input.modifiers.ctrl && input.key_pressed(Key::R) {
            *state = UiState::default();
            refresh();
        }
        if !state.loaded_entries.is_empty() {
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
        }
    });

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
            let entry_id = entry.entry.id();
            let response = match entry.cache.clone() {
                UiEntryCache::Text { one_liner } => {
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
                        if state.highlighted_id == Some(entry_id) {
                            response.scroll_to_me(None);
                        }
                    } else if response.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                        state.highlighted_id = Some(entry_id);
                    }
                    if state.highlighted_id == Some(entry_id) {
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
                    if response.secondary_clicked()
                        || (try_popup && state.highlighted_id == Some(entry_id))
                    {
                        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
                    }
                    egui::popup::popup_below_widget(ui, popup_id, &response, |ui| {
                        if state.details_requested != Some(entry_id) {
                            state.details_requested = Some(entry_id);
                            state.detailed_entry = None;
                            let _ = requests.send(Command::GetDetails {
                                entry: entry.entry,
                                with_text: true,
                            });
                        }

                        ui.set_min_width(200.);

                        ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                            ui.horizontal(|ui| {
                                match entry.entry.ring() {
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

                            ui.label(format!("Id: {}", entry_id));
                            match &state.detailed_entry {
                                None => {
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
                                        ScrollArea::both().auto_shrink([false, true]).show(
                                            ui,
                                            |ui| {
                                                ui.label(full);
                                            },
                                        );
                                    } else {
                                        ui.label("Binary data.");
                                    }
                                }
                                Some(Err(e)) => {
                                    ui.label(format!("Failed to get entry details:\n{e}"));
                                }
                            }
                        });
                    });

                    response
                }
                // TODO why is this so broken? Loads in weird sizes and doesn't work after the first
                //  load.
                UiEntryCache::Image { uri } => ui.add(Image::new(uri)),
                UiEntryCache::Binary { mime_type, context } => ui.label(format!(
                    "Unknown binary format of type {mime_type:?} from {context}."
                )),
                UiEntryCache::Error(e) => {
                    ui.label(e);
                    return;
                }
            };
            if response.clicked() {
                // TODO
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
