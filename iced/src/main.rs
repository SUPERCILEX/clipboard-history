use std::{
    env,
    fs::File,
    io::{self, BufReader},
    sync::{
        mpsc,
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use iced::{
    Background, Border, Color, Element, Length, Padding, Task, Subscription, Theme,
    keyboard::{self, key},
    widget::{
        button, column, container, image, row, scrollable, text, text_input,
    },
    Font, Shadow,
};
use ::image as image_crate;
use ringboard_sdk::{
    ClientError,
    core::{
        protocol::RingKind,
        Error as CoreError,
    },
    search::{CancellationTokenSink, cancellation_token},
    ui_actor::{
        Command, CommandError, DetailedEntry, Message, SearchKind, UiEntry, UiEntryCache,
        controller,
    },
};

#[cfg(feature = "trace")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

fn main() -> iced::Result {
    iced::application(
        RingboardApp::boot,
        RingboardApp::update,
        RingboardApp::view,
    )
    .title(|app: &RingboardApp| app.title())
    .subscription(RingboardApp::subscription)
    .theme(|_: &RingboardApp| Theme::Dracula)
    .run()
}

struct RingboardApp {
    requests: Sender<Command>,
    responses: Arc<Mutex<Receiver<Message>>>,
    state: State,
    image_cache: ImageCache,
    loaded_image_pending: LoadedImagePending,
}

type ImageCache = std::collections::HashMap<u64, image::Handle>;
type LoadedImagePending = std::collections::HashSet<u64>;

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
    detailed_entry: Option<DetailedEntry>,
    query: String,
    search_highlighted_id: Option<u64>,
    search_kind: SearchKind,
    pending_search_token: Option<CancellationTokenSink>,
    active_tab: ActiveTab,
    pinned_expanded: bool,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTab {
    #[default]
    All,
    Text,
    Images,
    Favorites,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum AppMessage {
    Tick,
    KeyEvent(keyboard::Event),
    ImageDecoded(u64, Result<image_crate::DynamicImage, String>),
    SearchChanged(String),
    SearchKindToggled,
    TabSelected(ActiveTab),
    PinnedToggled,
    EntryClicked(u64),
    FavoriteToggled(u64),
    DeleteEntry(u64),
    DetailRequested(u64),
    DetailClosed,
    Refresh,
    FastPaste(u64),
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

fn card_style(theme: &Theme, highlighted: bool, is_favorite: bool) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(if highlighted {
            palette.primary.weak.color
        } else {
            palette.background.weak.color
        })),
        border: Border {
            color: if is_favorite {
                palette.primary.strong.color
            } else {
                Color::TRANSPARENT
            },
            width: if is_favorite { 2.0 } else { 0.0 },
            radius: 6.0.into(),
        },
        shadow: Shadow::default(),
        text_color: None,
        snap: false,
    }
}

fn section_header_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.strong.color)),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        snap: false,
        ..container::Style::default()
    }
}

fn error_style(theme: &Theme) -> container::Style {
    let _ = theme;
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.35, 0.1, 0.1))),
        border: Border {
            color: Color::from_rgb(0.7, 0.2, 0.2),
            width: 1.0,
            radius: 4.0.into(),
        },
        text_color: Some(Color::from_rgb(1.0, 0.6, 0.6)),
        snap: false,
        ..container::Style::default()
    }
}

fn detail_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.strong.color)),
        border: Border {
            color: palette.background.weak.text,
            width: 1.0,
            radius: 6.0.into(),
        },
        snap: false,
        ..container::Style::default()
    }
}

impl RingboardApp {
    fn boot() -> (Self, Task<AppMessage>) {
        let (command_sender, command_receiver) = mpsc::channel();
        let (response_sender, response_receiver) = mpsc::sync_channel(8);
        let requests = command_sender.clone();
        let responses = Arc::new(Mutex::new(response_receiver));

        thread::spawn(move || {
            controller(&command_receiver, |m| response_sender.send(m).map_err(|_| ()));
        });

        let state = State::default();
        let app = RingboardApp {
            requests,
            responses,
            state,
            image_cache: ImageCache::default(),
            loaded_image_pending: LoadedImagePending::default(),
        };

        (app, Task::none())
    }

    fn title(&self) -> String {
        format!("Ringboard v{}", env!("CARGO_PKG_VERSION"))
    }

    fn update(&mut self, message: AppMessage) -> Task<AppMessage> {
        match message {
            AppMessage::Tick => self.poll_responses(),
            AppMessage::KeyEvent(event) => self.handle_key_event(event),
            AppMessage::ImageDecoded(id, result) => self.handle_image_decoded(id, result),
            AppMessage::SearchChanged(query) => self.handle_search_changed(query),
            AppMessage::SearchKindToggled => {
                self.state.ui.search_kind = match self.state.ui.search_kind {
                    SearchKind::Plain => SearchKind::Regex,
                    SearchKind::Regex => SearchKind::Mime,
                    SearchKind::Mime => SearchKind::Plain,
                };
                if !self.state.ui.query.is_empty() {
                    self.send_search()
                } else {
                    Task::none()
                }
            }
            AppMessage::TabSelected(tab) => {
                self.state.ui.active_tab = tab;
                Task::none()
            }
            AppMessage::PinnedToggled => {
                self.state.ui.pinned_expanded = !self.state.ui.pinned_expanded;
                Task::none()
            }
            AppMessage::EntryClicked(id) => {
                self.state.ui.pending_search_token.take();
                let _ = self.requests.send(Command::Paste(id));
                Task::none()
            }
            AppMessage::FavoriteToggled(id) => {
                let cmd = {
                    let entry = self.state.entries.loaded_entries
                        .iter()
                        .chain(self.state.entries.search_results.iter())
                        .find(|e| e.entry.id() == id);
                    match entry.map(|e| e.entry.ring()) {
                        Some(RingKind::Favorites) => Command::Unfavorite(id),
                        _ => Command::Favorite(id),
                    }
                };
                let _ = self.requests.send(cmd);
                self.refresh_entries()
            }
            AppMessage::DeleteEntry(id) => {
                let _ = self.requests.send(Command::Delete(id));
                if self.state.ui.query.is_empty() {
                    self.state.ui.highlighted_id = None;
                } else {
                    self.state.ui.search_highlighted_id = None;
                }
                self.refresh_entries()
            }
            AppMessage::DetailRequested(id) => {
                if self.state.ui.details_requested != Some(id) {
                    self.state.ui.details_requested = Some(id);
                    self.state.ui.detailed_entry = None;
                    let has_text = self.state.entries.loaded_entries
                        .iter()
                        .chain(self.state.entries.search_results.iter())
                        .any(|e| e.entry.id() == id && e.cache.is_text());
                    let _ = self.requests.send(Command::GetDetails { id, with_text: has_text });
                }
                Task::none()
            }
            AppMessage::DetailClosed => {
                self.state.ui.details_requested = None;
                self.state.ui.detailed_entry = None;
                Task::none()
            }
            AppMessage::Refresh => {
                self.state.ui.last_error.take();
                self.state.ui.highlighted_id = None;
                self.state.ui.search_highlighted_id = None;
                self.image_cache.clear();
                self.loaded_image_pending.clear();
                self.refresh_entries()
            }
            AppMessage::FastPaste(id) => {
                self.state.ui.pending_search_token.take();
                let _ = self.requests.send(Command::Paste(id));
                Task::none()
            }
        }
    }

    fn poll_responses(&mut self) -> Task<AppMessage> {
        let mut messages = Vec::new();
        while let Ok(msg) = self.responses.lock().unwrap().try_recv() {
            messages.push(msg);
        }

        let mut tasks: Vec<Task<AppMessage>> = Vec::new();
        for msg in messages {
            match msg {
                Message::LoadedImage { id, image } => {
                    if !self.loaded_image_pending.contains(&id) {
                        self.loaded_image_pending.insert(id);
                        tasks.push(Task::perform(
                            decode_image_async(id, image),
                            |(id, result)| AppMessage::ImageDecoded(id, result),
                        ));
                    }
                }
                other => tasks.push(self.handle_controller_message(other)),
            }
        }
        if tasks.is_empty() { Task::none() } else { Task::batch(tasks) }
    }

    fn handle_controller_message(&mut self, msg: Message) -> Task<AppMessage> {
        match msg {
            Message::FatalDbOpen(e) => {
                self.state.ui.fatal_error = Some(ClientError::Core(e));
                Task::none()
            }
            Message::Error(e) => {
                self.state.ui.last_error = Some(e);
                Task::none()
            }
            Message::LoadedFirstPage { entries: new_entries, default_focused_id } => {
                if self.state.ui.highlighted_id.is_none() {
                    self.state.ui.highlighted_id = default_focused_id;
                }
                self.state.entries.loaded_entries = new_entries;
                Task::none()
            }
            Message::EntryDetails { id, result } => {
                if self.state.ui.details_requested == Some(id) {
                    self.state.ui.detailed_entry = result.ok();
                }
                Task::none()
            }
            Message::SearchResults(new_entries) => {
                self.state.ui.search_highlighted_id = new_entries.first().map(|e| e.entry.id());
                self.state.entries.search_results = new_entries;
                Task::none()
            }
            Message::FavoriteChange(id) => {
                if self.state.ui.query.is_empty() {
                    self.state.ui.highlighted_id = Some(id);
                } else {
                    self.state.ui.search_highlighted_id = Some(id);
                }
                self.image_cache.remove(&id);
                self.refresh_entries()
            }
            Message::Deleted(_) => self.refresh_entries(),
            Message::LoadedImage { .. } => Task::none(),
            Message::Pasted => std::process::exit(0),
        }
    }

    fn handle_image_decoded(
        &mut self, id: u64, result: Result<image_crate::DynamicImage, String>,
    ) -> Task<AppMessage> {
        self.loaded_image_pending.remove(&id);
        match result {
            Ok(img) => {
                let max_w = 320;
                let (w, h) = (img.width(), img.height());
                let (nw, nh) = if w > max_w {
                    let ratio = max_w as f32 / w as f32;
                    (max_w, (h as f32 * ratio) as u32)
                } else { (w, h) };
                let thumb = img.thumbnail(nw, nh);
                let rgba = thumb.to_rgba8();
                let (tw, th) = rgba.dimensions();
                self.image_cache.insert(id, image::Handle::from_rgba(tw, th, rgba.into_raw()));
            }
            Err(e) => {
                self.state.ui.last_error = Some(CommandError::Core(
                    CoreError::Io { error: io::Error::other(e), context: "image decode".into() },
                ));
            }
        }
        Task::none()
    }

    fn handle_search_changed(&mut self, query: String) -> Task<AppMessage> {
        self.state.ui.last_error = None;
        if query.is_empty() {
            self.state.ui.query = String::new();
            self.state.entries.search_results = Box::default();
            self.state.ui.search_highlighted_id = None;
            self.state.ui.pending_search_token = None;
            return Task::none();
        }
        self.state.ui.query = query;
        self.send_search()
    }

    fn send_search(&mut self) -> Task<AppMessage> {
        let (source, sink) = cancellation_token();
        let _ = self.requests.send(Command::Search {
            query: self.state.ui.query.clone().into(),
            kind: self.state.ui.search_kind,
            token: source,
        });
        self.state.ui.pending_search_token = Some(sink);
        Task::none()
    }

    fn refresh_entries(&mut self) -> Task<AppMessage> {
        self.state.ui.last_error.take();
        let _ = self.requests.send(Command::LoadFirstPage);
        if !self.state.ui.query.is_empty() { self.send_search() } else { Task::none() }
    }

    fn handle_key_event(&mut self, event: keyboard::Event) -> Task<AppMessage> {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return Task::none();
        };

        let current_id = if self.state.ui.query.is_empty() {
            self.state.ui.highlighted_id
        } else {
            self.state.ui.search_highlighted_id
        };

        let show_sections = self.state.ui.query.is_empty()
            && self.state.ui.active_tab == ActiveTab::All;
        let nav = self.nav_entries();
        let pinned: Vec<&UiEntry> = self.filtered_entries().iter()
            .filter(|e| e.entry.ring() == RingKind::Favorites).copied().collect();
        let unpinned: Vec<&UiEntry> = self.filtered_entries().iter()
            .filter(|e| e.entry.ring() == RingKind::Main).copied().collect();

        let mut new_id = current_id;
        let mut set_pinned_expanded: Option<bool> = None;

        match &key {
            key::Key::Named(key::Named::ArrowUp) if !modifiers.control() => {
                new_id = if let Some(id) = current_id {
                    let idx = nav.iter().position(|e| e.entry.id() == id);
                    if idx == Some(0) || idx.is_none() {
                        nav.last().map(|e| e.entry.id())
                    } else {
                        idx.and_then(|i| i.checked_sub(1)).and_then(|i| nav.get(i)).map(|e| e.entry.id())
                    }
                } else {
                    nav.last().map(|e| e.entry.id())
                };
                if show_sections && !self.state.ui.pinned_expanded
                    && let Some(id) = new_id
                    && pinned.iter().any(|e| e.entry.id() == id)
                {
                    set_pinned_expanded = Some(true);
                }
            }
            key::Key::Named(key::Named::ArrowDown) if !modifiers.control() => {
                new_id = if let Some(id) = current_id {
                    let idx = nav.iter().position(|e| e.entry.id() == id);
                    if idx == Some(nav.len().saturating_sub(1)) || idx.is_none() {
                        nav.first().map(|e| e.entry.id())
                    } else {
                        idx.and_then(|i| i.checked_add(1)).and_then(|i| nav.get(i)).map(|e| e.entry.id())
                    }
                } else {
                    nav.first().map(|e| e.entry.id())
                };
                if show_sections && !self.state.ui.pinned_expanded
                    && let Some(id) = new_id
                    && pinned.iter().any(|e| e.entry.id() == id)
                {
                    set_pinned_expanded = Some(true);
                }
            }
            key::Key::Named(key::Named::ArrowLeft) => {
                if show_sections && !pinned.is_empty() {
                    set_pinned_expanded = Some(false);
                    new_id = unpinned.first().map(|e| e.entry.id());
                }
            }
            key::Key::Named(key::Named::ArrowRight) => {
                if show_sections && !pinned.is_empty() {
                    set_pinned_expanded = Some(true);
                }
            }
            key::Key::Named(key::Named::Enter) => {
                if let Some(id) = current_id {
                    self.state.ui.pending_search_token.take();
                    let _ = self.requests.send(Command::Paste(id));
                }
                std::process::exit(0);
            }
            key::Key::Named(key::Named::Escape) => {
                if self.state.ui.details_requested.is_some() {
                    return Task::done(AppMessage::DetailClosed);
                }
                if !self.state.ui.query.is_empty() {
                    self.state.ui.query = String::new();
                    self.state.entries.search_results = Box::default();
                    self.state.ui.highlighted_id = None;
                    self.state.ui.search_highlighted_id = None;
                    self.state.ui.last_error = None;
                    self.state.ui.pending_search_token = None;
                    return Task::none();
                }
                std::process::exit(0);
            }
            key::Key::Named(key::Named::Space) => {
                if let Some(id) = current_id {
                    if self.state.ui.details_requested == Some(id) {
                        return Task::done(AppMessage::DetailClosed);
                    }
                    return Task::done(AppMessage::DetailRequested(id));
                }
                return Task::none();
            }
            key::Key::Character(c) => {
                let s = c.as_str();
                if modifiers.control() && (s == "r" || s == "R") {
                    return Task::done(AppMessage::Refresh);
                }
                if modifiers.control() {
                    if let Some(digit) = s.chars().next().and_then(|c| c.to_digit(10)) {
                        let idx = digit as usize;
                        if let Some(entry) = nav.get(idx) {
                            return Task::done(AppMessage::FastPaste(entry.entry.id()));
                        }
                    }
                    return Task::none();
                }
                if modifiers.alt() && (s == "x" || s == "X" || s == "m" || s == "M") {
                    return Task::done(AppMessage::SearchKindToggled);
                }
                return Task::none();
            }
            _ => return Task::none(),
        }

        if self.state.ui.query.is_empty() {
            self.state.ui.highlighted_id = new_id;
        } else {
            self.state.ui.search_highlighted_id = new_id;
        }
        if let Some(expanded) = set_pinned_expanded {
            self.state.ui.pinned_expanded = expanded;
        }
        Task::none()
    }

    fn nav_entries(&self) -> Vec<&UiEntry> {
        let filtered = self.filtered_entries();
        if self.state.ui.query.is_empty() && self.state.ui.active_tab == ActiveTab::All {
            let pinned: Vec<_> = filtered.iter()
                .filter(|e| e.entry.ring() == RingKind::Favorites).copied().collect();
            let unpinned: Vec<_> = filtered.iter()
                .filter(|e| e.entry.ring() == RingKind::Main).copied().collect();
            pinned.into_iter().chain(unpinned).collect()
        } else {
            filtered
        }
    }

    fn subscription(&self) -> Subscription<AppMessage> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(16)).map(|_| AppMessage::Tick),
            keyboard::listen().map(AppMessage::KeyEvent),
        ])
    }

    fn view(&self) -> Element<'_, AppMessage> { self.view_impl() }

    fn view_impl<'a>(&'a self) -> Element<'a, AppMessage> {
        let filtered = self.filtered_entries();
        let show_sections = self.state.ui.query.is_empty()
            && self.state.ui.active_tab == ActiveTab::All;
        let pinned: Vec<&UiEntry> = filtered.iter()
            .filter(|e| e.entry.ring() == RingKind::Favorites).copied().collect();
        let unpinned: Vec<&UiEntry> = filtered.iter()
            .filter(|e| e.entry.ring() == RingKind::Main).copied().collect();
        let has_favorites = !pinned.is_empty();
        let detail_id = self.state.ui.details_requested;

        let mut col = column![self.search_bar(), self.tab_bar()].spacing(4);

        // Error display
        if let Some(ref e) = self.state.ui.fatal_error {
            col = col.push(
                container(text(format!("Fatal: {e}")).size(13))
                    .style(error_style).padding(8),
            );
        }
        if let Some(ref e) = self.state.ui.last_error {
            col = col.push(
                container(text(format!("Error: {e:#?}")).size(11))
                    .style(error_style).padding(6),
            );
        }

        col = col.push(self.entry_list(
            &filtered, show_sections, has_favorites, &pinned, &unpinned, detail_id,
        ));

        // Fast paste hint bar
        if show_sections {
            let nav = self.nav_entries();
            let shortcuts: Vec<Element<AppMessage>> = nav.iter().take(10).enumerate()
                .map(|(i, entry)| {
                    button(text(format!("{}", i)).size(10))
                        .on_press(AppMessage::FastPaste(entry.entry.id()))
                        .style(button::secondary)
                        .padding(2)
                        .into()
                })
                .collect();

            if !shortcuts.is_empty() {
                col = col.push(
                    row![
                        text("Ctrl+").size(10),
                        row(shortcuts).spacing(2),
                    ].spacing(4).padding(Padding::new(2.0)),
                );
            }
        }

        container(col).width(Length::Fill).height(Length::Fill).padding(8).into()
    }

    fn search_bar<'a>(&'a self) -> Element<'a, AppMessage> {
        let hint = match self.state.ui.search_kind {
            SearchKind::Plain => "Search...",
            SearchKind::Regex => "RegEx search...",
            SearchKind::Mime => "MIME search...",
        };
        let kind_label = match self.state.ui.search_kind {
            SearchKind::Plain => "~",
            SearchKind::Regex => ".*",
            SearchKind::Mime => "MIME",
        };

        container(
            row![
                text_input(hint, &self.state.ui.query)
                    .on_input(AppMessage::SearchChanged)
                    .width(Length::Fill)
                    .size(14)
                    .font(Font::MONOSPACE),
                button(text(kind_label).size(11).font(Font::MONOSPACE))
                    .on_press(AppMessage::SearchKindToggled)
                    .style(button::secondary)
                    .padding(Padding::new(4.0)),
            ].spacing(6)
        )
        .padding(Padding::new(4.0))
        .style(|theme: &Theme| {
            let palette = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(palette.background.strong.color)),
                border: Border { radius: 6.0.into(), ..Border::default() },
                snap: false,
                ..container::Style::default()
            }
        })
        .into()
    }

    fn tab_bar<'a>(&'a self) -> Element<'a, AppMessage> {
        let tabs = [ActiveTab::All, ActiveTab::Text, ActiveTab::Images, ActiveTab::Favorites];
        let labels = ["All", "Text", "Images", "Pins"];
        let current = self.state.ui.active_tab;

        let buttons: Vec<Element<AppMessage>> = tabs.iter().zip(labels.iter())
            .map(|(tab, label)| {
                let mut b = button(text(*label).size(13));
                if current == *tab { b = b.style(button::primary); }
                b.on_press(AppMessage::TabSelected(*tab)).into()
            })
            .collect();

        row(buttons).spacing(4).into()
    }

    fn entry_list<'a>(
        &'a self, filtered: &[&'a UiEntry], show_sections: bool, has_favorites: bool,
        pinned: &[&'a UiEntry], unpinned: &[&'a UiEntry], detail_id: Option<u64>,
    ) -> Element<'a, AppMessage> {
        let render_items: Vec<&UiEntry> = if show_sections {
            unpinned.to_vec()
        } else {
            filtered.to_vec()
        };

        if render_items.is_empty() && pinned.is_empty()
            && self.state.ui.pending_search_token.is_none()
        {
            return container(text("Nothing to see here").size(16))
                .width(Length::Fill).height(Length::Fill)
                .center_x(Length::Fill).center_y(Length::Fill).into();
        }

        let mut col = column![].spacing(2);

        if show_sections && has_favorites {
            col = col.push(
                container(row![
                    button(text(if self.state.ui.pinned_expanded {
                        "\u{25BC}"
                    } else {
                        "\u{25B6}"
                    }).size(12))
                        .on_press(AppMessage::PinnedToggled)
                        .style(button::text),
                    text("Pinned").size(13),
                    container(text(pinned.len().to_string()).size(11))
                        .style(|theme: &Theme| container::Style {
                            background: Some(Background::Color(
                                theme.extended_palette().primary.weak.color
                            )),
                            border: Border { radius: 8.0.into(), ..Border::default() },
                            snap: false,
                            ..container::Style::default()
                        })
                        .padding(Padding::new(2.0).left(6).right(6)),
                ].spacing(6).align_y(iced::Alignment::Center))
                .style(section_header_style)
                .padding(Padding::new(6.0)),
            );

            if self.state.ui.pinned_expanded {
                for entry in pinned {
                    col = col.push(self.entry_card(entry, detail_id));
                }
            }

            if !unpinned.is_empty() {
                col = col.push(
                    container(row![
                        text("Recent").size(13),
                        container(text(unpinned.len().to_string()).size(11))
                            .style(|theme: &Theme| container::Style {
                                background: Some(Background::Color(
                                    theme.extended_palette().background.strong.color
                                )),
                                border: Border { radius: 8.0.into(), ..Border::default() },
                                snap: false,
                                ..container::Style::default()
                            })
                            .padding(Padding::new(2.0).left(6).right(6)),
                    ].spacing(6).align_y(iced::Alignment::Center))
                    .style(section_header_style)
                    .padding(Padding::new(6.0)),
                );
            }
        }

        for entry in render_items {
            col = col.push(self.entry_card(entry, detail_id));
        }

        scrollable(col).height(Length::Fill).into()
    }

    fn entry_card<'a>(
        &'a self, entry: &'a UiEntry, detail_id: Option<u64>,
    ) -> Element<'a, AppMessage> {
        let id = entry.entry.id();
        let is_favorite = entry.entry.ring() == RingKind::Favorites;
        let is_highlighted = if self.state.ui.query.is_empty() {
            self.state.ui.highlighted_id == Some(id)
        } else {
            self.state.ui.search_highlighted_id == Some(id)
        };
        let is_detail_open = detail_id == Some(id);

        let content_preview: Element<AppMessage> = match &entry.cache {
            UiEntryCache::Text { one_liner }
            | UiEntryCache::HighlightedText { one_liner, .. } => {
                let display = if one_liner.len() > 200 {
                    &one_liner[..200]
                } else {
                    one_liner
                };
                column![text(display).font(Font::MONOSPACE).size(13).width(Length::Fill)]
                    .spacing(0).padding(0).into()
            }
            UiEntryCache::Image => {
                if let Some(handle) = self.image_cache.get(&id) {
                    row![
                        image(handle.clone())
                            .width(Length::Fixed(60.0)).height(Length::Fixed(60.0)),
                        text("Image").size(13),
                    ].spacing(8).align_y(iced::Alignment::Center).into()
                } else {
                    if !self.loaded_image_pending.contains(&id) {
                        let _ = self.requests.send(Command::LoadImage(id));
                    }
                    row![text("Image (loading...)").size(13)].into()
                }
            }
            UiEntryCache::Binary { mime_type } => {
                column![text(format!("[{}]", mime_type)).size(13).font(Font::MONOSPACE)].spacing(0).padding(0).into()
            }
            UiEntryCache::Error(_) => {
                column![text("Error").size(13)].spacing(0).padding(0).into()
            }
        };

        let action_row = row![
            button(text(if is_favorite { "\u{2605}" } else { "\u{2606}" }).size(14))
                .on_press(AppMessage::FavoriteToggled(id))
                .style(button::text).padding(4),
            button(text("\u{2715}").size(12))
                .on_press(AppMessage::DeleteEntry(id))
                .style(button::text).padding(4),
            button(text(if is_detail_open { "\u{25B2}" } else { "\u{25BC}" }).size(10))
                .on_press(if is_detail_open {
                    AppMessage::DetailClosed
                } else {
                    AppMessage::DetailRequested(id)
                })
                .style(button::text).padding(4),
        ].spacing(2);

        let mut card_col = column![
            row![content_preview, action_row].align_y(iced::Alignment::Center),
        ].spacing(4);

        // Detail panel
        if is_detail_open {
            let detail_content: Element<AppMessage> = match &self.state.ui.detailed_entry {
                None => text("Loading details...").size(13).into(),
                Some(DetailedEntry { mime_type, full_text }) => {
                    let mut detail_col = column![].spacing(4);
                    if !mime_type.is_empty() {
                        detail_col = detail_col.push(
                            text(format!("MIME: {}", mime_type)).size(12).font(Font::MONOSPACE),
                        );
                    }
                    if let Some(full) = full_text {
                        detail_col = detail_col.push(
                            scrollable(text(&**full).font(Font::MONOSPACE).size(12))
                                .height(Length::Fixed(200.0)),
                        );
                    } else if matches!(&entry.cache, UiEntryCache::Image) {
                        if let Some(handle) = self.image_cache.get(&id) {
                            detail_col = detail_col.push(
                                scrollable(image(handle.clone()))
                                    .height(Length::Fixed(300.0)),
                            );
                        }
                    } else {
                        detail_col = detail_col.push(text("Binary data").size(12));
                    }
                    detail_col.into()
                }
            };

            card_col = card_col.push(
                container(detail_content).style(detail_style).padding(8).width(Length::Fill),
            );
        }

        container(card_col)
            .style(move |theme| card_style(theme, is_highlighted, is_favorite))
            .padding(8)
            .width(Length::Fill)
            .into()
    }

    fn filtered_entries(&self) -> Vec<&UiEntry> {
        let raw = active_entries!(self.state.entries, self.state.ui);
        raw.iter().filter(|e| match self.state.ui.active_tab {
            ActiveTab::All => true,
            ActiveTab::Text => matches!(
                e.cache,
                UiEntryCache::Text { .. } | UiEntryCache::HighlightedText { .. }
            ),
            ActiveTab::Images => matches!(e.cache, UiEntryCache::Image),
            ActiveTab::Favorites => e.entry.ring() == RingKind::Favorites,
        }).collect()
    }
}

async fn decode_image_async(
    id: u64, file: File,
) -> (u64, Result<image_crate::DynamicImage, String>) {
    let result = tokio::task::spawn_blocking(move || {
        image_crate::ImageReader::new(BufReader::new(file))
            .with_guessed_format()
            .map_err(|e| format!("Failed to guess format: {e}"))
            .and_then(|r| r.decode().map_err(|e| format!("Failed to decode: {e}")))
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task panicked: {e}")));
    (id, result)
}
