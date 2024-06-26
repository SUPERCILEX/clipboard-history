use std::{
    cmp::{min, Ordering, Reverse},
    collections::{BinaryHeap, HashMap},
    hash::BuildHasherDefault,
    iter::once,
    mem,
    os::fd::{AsFd, BorrowedFd},
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
        text::LayoutJob, Align, CentralPanel, FontId, Image, InputState, Key, Label, Layout, Pos2,
        Response, ScrollArea, Sense, TextEdit, TextFormat, TopBottomPanel, Ui, Vec2,
        ViewportBuilder, Widget,
    },
    epaint::FontFamily,
};
use regex::bytes::Regex;
use ringboard_sdk::{
    connect_to_server,
    core::{
        direct_file_name,
        dirs::{data_dir, socket_file},
        protocol::{IdNotFoundError, MoveToFrontResponse, RemoveResponse, RingKind},
        ring::{offset_to_entries, Ring},
        size_to_bucket, Error as CoreError, IoErr, PathView,
    },
    duplicate_detection::RingAndIndex,
    search::{BucketAndIndex, EntryLocation, Query},
    ClientError, DatabaseReader, Entry, EntryReader, Kind,
};
use rustc_hash::FxHasher;
use rustix::{
    fs::{openat, statx, AtFlags, Mode, OFlags, StatxFlags, CWD},
    net::SocketAddrUnix,
    process::fchdir,
};
use thiserror::Error;

#[derive(Error, Debug)]
enum CommandError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Sdk(#[from] ClientError),
    #[error("Regex instantiation failed.")]
    Regex(#[from] regex::Error),
}

impl From<IdNotFoundError> for CommandError {
    fn from(value: IdNotFoundError) -> Self {
        Self::Core(CoreError::IdNotFound(value))
    }
}

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
                move || controller(&ctx, &command_receiver, &response_sender)
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
    Search { query: String, regex: bool },
}

#[derive(Debug)]
enum Message {
    FatalDbOpen(CoreError),
    FatalServerConnect(ClientError),
    Error(CommandError),
    LoadedFirstPage {
        entries: Vec<UiEntry>,
        first_non_favorite_id: Option<u64>,
    },
    EntryDetails(Result<DetailedEntry, CoreError>),
    SearchResults(Vec<UiEntry>),
}

fn controller(ctx: &egui::Context, commands: &Receiver<Command>, responses: &SyncSender<Message>) {
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
    let ((mut database, reader), rings) = {
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
    let mut reader = Some(reader);
    let mut reverse_index_cache = HashMap::default();

    for command in once(Command::LoadFirstPage).chain(commands) {
        let result = handle_command(
            command,
            (&server.0, &server.1),
            &mut database,
            &mut reader,
            &(&rings.0, &rings.1),
            &mut reverse_index_cache,
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

#[allow(clippy::too_many_lines)]
fn handle_command(
    command: Command,
    server: (impl AsFd, &SocketAddrUnix),
    database: &mut DatabaseReader,
    reader_: &mut Option<EntryReader>,
    rings: &(impl AsFd, impl AsFd),
    reverse_index_cache: &mut HashMap<BucketAndIndex, RingAndIndex, BuildHasherDefault<FxHasher>>,
) -> Result<Option<Message>, CommandError> {
    let reader = reader_.as_mut().unwrap();
    match command {
        Command::RefreshDb => {
            reverse_index_cache.clear();
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
        Command::Search { mut query, regex } => {
            if reverse_index_cache.is_empty() {
                for entry in database.favorites().chain(database.main()) {
                    let Kind::Bucket(bucket) = entry.kind() else {
                        continue;
                    };
                    reverse_index_cache.insert(
                        BucketAndIndex::new(size_to_bucket(bucket.size()), bucket.index()),
                        RingAndIndex::new(entry.ring(), entry.index()),
                    );
                }
            }

            let query = if regex {
                Query::Regex(Regex::new(&query)?)
            } else if query.chars().all(char::is_lowercase) {
                query.make_ascii_lowercase();
                Query::PlainIgnoreCase(query.trim().as_bytes())
            } else {
                Query::Plain(query.trim().as_bytes())
            };
            Ok(Some(Message::SearchResults(do_search(
                query,
                reader_,
                database,
                reverse_index_cache,
            ))))
        }
    }
}

fn do_search(
    query: Query,
    reader_: &mut Option<EntryReader>,
    database: &mut DatabaseReader,
    reverse_index_cache: &HashMap<BucketAndIndex, RingAndIndex, BuildHasherDefault<FxHasher>>,
) -> Vec<UiEntry> {
    const MAX_SEARCH_ENTRIES: usize = 256;

    struct SortedEntry(Entry);

    // TODO fix this being broken when ring wraps around, need to take into account
    //  the write_head
    impl PartialOrd for SortedEntry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for SortedEntry {
        fn cmp(&self, other: &Self) -> Ordering {
            self.0.id().cmp(&other.0.id())
        }
    }

    impl PartialEq<Self> for SortedEntry {
        fn eq(&self, other: &Self) -> bool {
            self.0.id() == other.0.id()
        }
    }
    impl Eq for SortedEntry {}

    let reader = Arc::new(reader_.take().unwrap());

    let (result_stream, threads) = ringboard_sdk::search(query, reader.clone());
    let mut results = BinaryHeap::new();
    for entry in result_stream
        .map(|r| {
            r.and_then(|q| match q.location {
                EntryLocation::Bucketed { bucket, index } => reverse_index_cache
                    .get(&BucketAndIndex::new(bucket, index))
                    .copied()
                    .ok_or_else(|| {
                        CoreError::IdNotFound(IdNotFoundError::Entry(
                            index << u8::BITS | u32::from(bucket),
                        ))
                    })
                    .and_then(|entry| {
                        unsafe { database.get(entry.id()) }.map_err(CoreError::IdNotFound)
                    }),
                EntryLocation::File { entry_id } => {
                    unsafe { database.get(entry_id) }.map_err(CoreError::IdNotFound)
                }
            })
        })
        .filter_map(Result::ok)
        .map(SortedEntry)
        .map(Reverse)
    {
        results.push(entry);
        if results.len() == MAX_SEARCH_ENTRIES {
            results.pop();
        }
    }

    for thread in threads {
        let _ = thread.join();
    }
    *reader_ = Some(Arc::into_inner(reader).unwrap());
    let reader = reader_.as_mut().unwrap();

    results
        .into_sorted_vec()
        .into_iter()
        .map(|entry| entry.0.0)
        .map(|entry| {
            // TODO add support for bold highlighting the selection range
            ui_entry(entry, reader).unwrap_or_else(|e| UiEntry {
                cache: UiEntryCache::Error(format!("Error: failed to load entry {entry:?}\n{e:?}")),
                entry,
            })
        })
        .collect()
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
    entries: UiEntries,
}

#[derive(Default)]
struct UiEntries {
    loaded_entries: Vec<UiEntry>,
    search_results: Vec<UiEntry>,
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

            entries: UiEntries::default(),
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
    } else if let Ok(s) = {
        let mut shrunk = &loaded[..min(loaded.len(), 250)];
        loop {
            let Some(&b) = shrunk.last() else {
                break;
            };
            // https://github.com/rust-lang/rust/blob/33422e72c8a66bdb5ee21246a948a1a02ca91674/library/core/src/num/mod.rs#L1090
            #[allow(clippy::cast_possible_wrap)]
            let is_utf8_char_boundary = (b as i8) >= -0x40;
            if is_utf8_char_boundary || loaded.len() == shrunk.len() {
                break;
            }

            shrunk = &loaded[..=shrunk.len()];
        }
        str::from_utf8(shrunk)
    } {
        let mut one_liner = String::new();
        let mut prev_char_is_whitespace = false;
        for c in s.chars() {
            if (prev_char_is_whitespace || one_liner.is_empty()) && c.is_whitespace() {
                continue;
            }

            one_liner.push(if c.is_whitespace() { ' ' } else { c });
            prev_char_is_whitespace = c.is_whitespace();
        }
        if s.len() != loaded.len() {
            one_liner.push('…');
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

fn handle_message(message: Message, ui_entries: &mut UiEntries, state: &mut UiState) {
    match message {
        Message::FatalDbOpen(e) => state.fatal_error = Some(e.into()),
        Message::FatalServerConnect(e) => state.fatal_error = Some(e),
        Message::Error(e) => state.last_error = Some(e),
        Message::LoadedFirstPage {
            entries,
            first_non_favorite_id,
        } => {
            ui_entries.loaded_entries = entries;
            if state.highlighted_id.is_none() {
                state.highlighted_id = first_non_favorite_id;
            }
        }
        Message::EntryDetails(r) => state.detailed_entry = Some(r),
        Message::SearchResults(entries) => {
            state.search_highlighted_id = entries.first().map(|e| e.entry.id());
            ui_entries.search_results = entries;
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
    entries: &mut UiEntries,
    state: &mut UiState,
    requests: &Sender<Command>,
) {
    let response = ui.add(
        TextEdit::singleline(&mut state.query)
            .hint_text(if state.search_with_regex {
                "Search with RegEx"
            } else {
                "Search"
            })
            .desired_width(f32::INFINITY)
            .cursor_at_end(true),
    );
    let mut reset = |state: &mut UiState| {
        state.query = String::new();
        entries.search_results = Vec::new();
        state.search_highlighted_id = None;
    };

    if ui.input(|input| input.key_pressed(Key::Escape)) && ui.memory(|mem| !mem.any_popup_open()) {
        reset(state);
    }
    if ui.input(|i| i.key_pressed(Key::ArrowUp) || i.key_pressed(Key::ArrowDown)) {
        response.surrender_focus();
    }
    if ui.input(|input| input.key_pressed(Key::Slash)) {
        ui.memory_mut(egui::Memory::close_popup);
        response.request_focus();
    }
    if ui.input(|i| i.modifiers.alt && i.key_pressed(Key::X)) {
        state.search_with_regex ^= true;
    }

    if !response.changed() {
        return;
    }
    if state.query.is_empty() {
        reset(state);
        return;
    }

    let _ = requests.send(Command::Search {
        query: state.query.clone(),
        regex: state.search_with_regex,
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

    if ui.input(|input| input.modifiers.ctrl && input.key_pressed(Key::R)) {
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
            Image::new(uri).max_height(250.).fit_to_original_size(1.),
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
            ui.label(e);
            return;
        }
    };
    if response.clicked() {
        // TODO
    }
}

#[allow(clippy::too_many_arguments)]
fn row_ui(
    ui: &mut Ui,
    widget: impl Widget,
    state: &mut UiState,
    requests: &Sender<Command>,
    mut refresh: impl FnMut(),
    entry: &UiEntry,
    try_scroll: bool,
    try_popup: bool,
) -> Response {
    let entry_id = entry.entry.id();

    let frame_data = egui::Frame::default().inner_margin(5.);
    let mut frame = frame_data.begin(ui);
    frame.content_ui.add(widget);
    frame
        .content_ui
        .set_min_width(frame.content_ui.available_width() - frame_data.inner_margin.right);
    let response = ui.allocate_rect(
        (frame_data.inner_margin + frame_data.outer_margin)
            .expand_rect(frame.content_ui.min_rect()),
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
    egui::popup::popup_below_widget(ui, popup_id, &response, |ui| {
        if state.details_requested != Some(entry_id) {
            state.details_requested = Some(entry_id);
            state.detailed_entry = None;
            let _ = requests.send(Command::GetDetails {
                entry: entry.entry,
                with_text: matches!(entry.cache, UiEntryCache::Text { .. }),
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

            ui.label(format!("Id: {entry_id}"));
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
                        ScrollArea::both()
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.label(full);
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
    });
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
