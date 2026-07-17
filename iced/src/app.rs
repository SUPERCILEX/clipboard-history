use std::{
    env, io,
    sync::{
        Arc, Mutex, mpsc,
        mpsc::{Receiver, Sender},
    },
    thread,
    time::Duration,
};

use ::image as image_crate;
use iced::{
    Element, Subscription, Task,
    keyboard::{self, key},
    widget::image,
};
use ringboard_sdk::{
    ClientError,
    core::{Error as CoreError, protocol::RingKind},
    search::cancellation_token,
    ui_actor::{
        Command, CommandError, Message as ControllerMessage, SearchKind, UiEntry, UiEntryCache,
        controller,
    },
};

use crate::message::Message;
use crate::state::{ActiveTab, State};
use crate::utils::decode_image_async;

pub type ImageCache = std::collections::HashMap<u64, image::Handle>;
pub type LoadedImagePending = std::collections::HashSet<u64>;

/// The application model plus communication channels (TEA Model).
pub struct RingboardApp {
    pub requests: Sender<Command>,
    pub responses: Arc<Mutex<Receiver<ControllerMessage>>>,
    pub state: State,
    pub image_cache: ImageCache,
    pub loaded_image_pending: LoadedImagePending,
}

impl RingboardApp {
    /// Initialize the model and spawn the background controller thread.
    pub fn boot() -> (Self, Task<Message>) {
        let (command_sender, command_receiver) = mpsc::channel();
        let (response_sender, response_receiver) = mpsc::sync_channel(8);
        let requests = command_sender.clone();
        let responses = Arc::new(Mutex::new(response_receiver));

        thread::spawn(move || {
            controller(&command_receiver, |m| {
                response_sender.send(m).map_err(|_| ())
            });
        });

        let state = State::new();
        let app = RingboardApp {
            requests,
            responses,
            state,
            image_cache: ImageCache::default(),
            loaded_image_pending: LoadedImagePending::default(),
        };

        (app, Task::none())
    }

    pub fn title(&self) -> String {
        format!("Ringboard v{}", env!("CARGO_PKG_VERSION"))
    }

    /// The TEA update function: (Model, Msg) -> (Model, Cmd).
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => self.poll_responses(),
            Message::KeyEvent(event) => self.handle_key_event(event),
            Message::ImageDecoded(id, result) => self.handle_image_decoded(id, result),
            Message::SearchChanged(query) => self.handle_search_changed(query),
            Message::SearchKindToggled => self.toggle_search_kind(),
            Message::TabSelected(tab) => self.select_tab(tab),
            Message::TabNext => self.cycle_tab(1),
            Message::TabPrev => self.cycle_tab(-1),
            Message::PinnedToggled => {
                self.state.ui.pinned_expanded = !self.state.ui.pinned_expanded;
                Task::none()
            }
            Message::EntryClicked(id) => self.paste(id),
            Message::FavoriteToggled(id) => self.toggle_favorite(id),
            Message::DeleteEntry(id) => self.delete(id),
            Message::DetailRequested(id) => self.open_detail(id),
            Message::DetailClosed => {
                self.state.ui.details_requested = None;
                self.state.ui.detailed_entry = None;
                Task::none()
            }

            Message::Refresh => self.refresh(),
            Message::FastPaste(id) => self.paste(id),
            Message::DismissError => {
                self.state.ui.last_error = None;
                Task::none()
            }
        }
    }

    /// The TEA view function: Model -> Html/Element.
    pub fn view(&self) -> Element<'_, Message> {
        crate::widgets::main_view(self)
    }

    /// The TEA subscriptions: Model -> Subscriptions.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(16)).map(|_| Message::Tick),
            keyboard::listen().map(Message::KeyEvent),
        ])
    }

    // ------------------------------------------------------------------
    // Query / read-only helpers (safe to call from view)
    // ------------------------------------------------------------------

    /// Return the entries currently visible based on search state.
    pub fn active_entries(&self) -> &[UiEntry] {
        if self.state.ui.query.is_empty() {
            &self.state.entries.loaded_entries
        } else {
            &self.state.entries.search_results
        }
    }

    /// Return entries filtered by the active tab.
    pub fn filtered_entries(&self) -> Vec<&UiEntry> {
        self.active_entries()
            .iter()
            .filter(|e| match self.state.ui.active_tab {
                ActiveTab::All => true,
                ActiveTab::Text => matches!(
                    e.cache,
                    UiEntryCache::Text { .. } | UiEntryCache::HighlightedText { .. }
                ),
                ActiveTab::Images => matches!(e.cache, UiEntryCache::Image),
                ActiveTab::Favorites => e.entry.ring() == RingKind::Favorites,
            })
            .collect()
    }

    /// Navigation order: pinned first (if shown), then recent/filtered.
    pub fn nav_entries(&self) -> Vec<&UiEntry> {
        let filtered = self.filtered_entries();
        if self.state.ui.query.is_empty() && self.state.ui.active_tab == ActiveTab::All {
            let pinned: Vec<_> = filtered
                .iter()
                .filter(|e| e.entry.ring() == RingKind::Favorites)
                .copied()
                .collect();
            let unpinned: Vec<_> = filtered
                .iter()
                .filter(|e| e.entry.ring() == RingKind::Main)
                .copied()
                .collect();
            pinned.into_iter().chain(unpinned).collect()
        } else {
            filtered
        }
    }

    pub fn current_highlight_id(&self) -> Option<u64> {
        if self.state.ui.query.is_empty() {
            self.state.ui.highlighted_id
        } else {
            self.state.ui.search_highlighted_id
        }
    }

    // ------------------------------------------------------------------
    // Commands / side effects (only called from update)
    // ------------------------------------------------------------------

    fn paste(&mut self, id: u64) -> Task<Message> {
        self.state.ui.pending_search_token.take();
        let _ = self.requests.send(Command::Paste(id));
        Task::none()
    }

    fn toggle_favorite(&mut self, id: u64) -> Task<Message> {
        let cmd = {
            let entry = self
                .state
                .entries
                .loaded_entries
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

    fn delete(&mut self, id: u64) -> Task<Message> {
        let _ = self.requests.send(Command::Delete(id));
        if self.state.ui.query.is_empty() {
            self.state.ui.highlighted_id = None;
        } else {
            self.state.ui.search_highlighted_id = None;
        }
        self.refresh_entries()
    }

    fn open_detail(&mut self, id: u64) -> Task<Message> {
        if self.state.ui.details_requested != Some(id) {
            self.state.ui.details_requested = Some(id);
            self.state.ui.detailed_entry = None;
            let entry = self
                .state
                .entries
                .loaded_entries
                .iter()
                .chain(self.state.entries.search_results.iter())
                .find(|e| e.entry.id() == id);
            let has_text = entry.is_some_and(|e| e.cache.is_text());
            let is_image = entry.is_some_and(|e| matches!(e.cache, UiEntryCache::Image));
            let _ = self.requests.send(Command::GetDetails {
                id,
                with_text: has_text,
            });
            if is_image {
                self.request_image(id);
            }
        }
        Task::none()
    }

    fn toggle_detail(&mut self) -> Task<Message> {
        if let Some(id) = self.current_highlight_id() {
            if self.state.ui.details_requested == Some(id) {
                return Task::done(Message::DetailClosed);
            }
            return Task::done(Message::DetailRequested(id));
        }
        Task::none()
    }

    fn select_tab(&mut self, tab: ActiveTab) -> Task<Message> {
        self.state.ui.active_tab = tab;
        self.state.ui.highlighted_id = None;
        self.state.ui.search_highlighted_id = None;
        Task::none()
    }

    fn cycle_tab(&mut self, delta: i8) -> Task<Message> {
        let tabs = ActiveTab::ALL;
        let current = tabs
            .iter()
            .position(|t| *t == self.state.ui.active_tab)
            .unwrap_or(0);
        let len = tabs.len() as i8;
        let next = (((current as i8 + delta) % len + len) % len) as usize;
        self.select_tab(tabs[next])
    }

    fn toggle_search_kind(&mut self) -> Task<Message> {
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

    fn refresh(&mut self) -> Task<Message> {
        self.state.ui.last_error.take();
        self.state.ui.highlighted_id = None;
        self.state.ui.search_highlighted_id = None;
        self.image_cache.clear();
        self.loaded_image_pending.clear();
        self.refresh_entries()
    }

    fn refresh_entries(&mut self) -> Task<Message> {
        self.state.ui.last_error.take();
        let _ = self.requests.send(Command::LoadFirstPage);
        if !self.state.ui.query.is_empty() {
            self.send_search()
        } else {
            Task::none()
        }
    }

    fn send_search(&mut self) -> Task<Message> {
        let (source, sink) = cancellation_token();
        let _ = self.requests.send(Command::Search {
            query: self.state.ui.query.clone().into(),
            kind: self.state.ui.search_kind,
            token: source,
        });
        self.state.ui.pending_search_token = Some(sink);
        Task::none()
    }

    fn handle_search_changed(&mut self, query: String) -> Task<Message> {
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

    fn handle_image_decoded(
        &mut self,
        id: u64,
        result: Result<image_crate::DynamicImage, String>,
    ) -> Task<Message> {
        self.loaded_image_pending.remove(&id);
        match result {
            Ok(img) => {
                let max_w = 320;
                let (w, h) = (img.width(), img.height());
                let (nw, nh) = if w > max_w {
                    let ratio = max_w as f32 / w as f32;
                    (max_w, (h as f32 * ratio) as u32)
                } else {
                    (w, h)
                };
                let thumb = img.thumbnail(nw, nh);
                let rgba = thumb.to_rgba8();
                let (tw, th) = rgba.dimensions();
                self.image_cache
                    .insert(id, image::Handle::from_rgba(tw, th, rgba.into_raw()));
            }
            Err(e) => {
                self.state.ui.last_error = Some(CommandError::Core(CoreError::Io {
                    error: io::Error::other(e),
                    context: "image decode".into(),
                }));
            }
        }
        Task::none()
    }

    // ------------------------------------------------------------------
    // Controller message handling
    // ------------------------------------------------------------------

    fn poll_responses(&mut self) -> Task<Message> {
        let mut messages = Vec::new();
        while let Ok(msg) = self.responses.lock().unwrap().try_recv() {
            messages.push(msg);
        }

        let mut tasks: Vec<Task<Message>> = Vec::new();
        for msg in messages {
            match msg {
                ControllerMessage::LoadedImage { id, image } => {
                    if !self.loaded_image_pending.contains(&id) {
                        self.loaded_image_pending.insert(id);
                        tasks.push(Task::perform(
                            decode_image_async(id, image),
                            |(id, result)| Message::ImageDecoded(id, result),
                        ));
                    }
                }
                other => tasks.push(self.handle_controller_message(other)),
            }
        }
        if tasks.is_empty() {
            Task::none()
        } else {
            Task::batch(tasks)
        }
    }

    fn handle_controller_message(&mut self, msg: ControllerMessage) -> Task<Message> {
        match msg {
            ControllerMessage::FatalDbOpen(e) => {
                self.state.ui.fatal_error = Some(ClientError::Core(e));
                Task::none()
            }
            ControllerMessage::Error(e) => {
                self.state.ui.last_error = Some(e);
                Task::none()
            }
            ControllerMessage::LoadedFirstPage {
                entries: new_entries,
                default_focused_id,
            } => {
                if self.state.ui.highlighted_id.is_none() {
                    self.state.ui.highlighted_id =
                        default_focused_id.or_else(|| new_entries.first().map(|e| e.entry.id()));
                }
                self.request_images(&new_entries);
                self.state.entries.loaded_entries = new_entries;
                Task::none()
            }
            ControllerMessage::EntryDetails { id, result } => {
                if self.state.ui.details_requested == Some(id) {
                    self.state.ui.detailed_entry = result.ok();
                }
                Task::none()
            }
            ControllerMessage::SearchResults(new_entries) => {
                self.state.ui.search_highlighted_id = new_entries.first().map(|e| e.entry.id());
                self.request_images(&new_entries);
                self.state.entries.search_results = new_entries;
                Task::none()
            }
            ControllerMessage::FavoriteChange(id) => {
                if self.state.ui.query.is_empty() {
                    self.state.ui.highlighted_id = Some(id);
                } else {
                    self.state.ui.search_highlighted_id = Some(id);
                }
                self.image_cache.remove(&id);
                self.refresh_entries()
            }
            ControllerMessage::Deleted(_) => self.refresh_entries(),
            ControllerMessage::LoadedImage { .. } => Task::none(),
            ControllerMessage::Pasted => std::process::exit(0),
        }
    }

    // ------------------------------------------------------------------
    // Keyboard handling
    // ------------------------------------------------------------------

    fn handle_key_event(&mut self, event: keyboard::Event) -> Task<Message> {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return Task::none();
        };

        let show_sections =
            self.state.ui.query.is_empty() && self.state.ui.active_tab == ActiveTab::All;
        let nav = self.nav_entries();
        let pinned: Vec<&UiEntry> = self
            .filtered_entries()
            .iter()
            .filter(|e| e.entry.ring() == RingKind::Favorites)
            .copied()
            .collect();
        let unpinned: Vec<&UiEntry> = self
            .filtered_entries()
            .iter()
            .filter(|e| e.entry.ring() == RingKind::Main)
            .copied()
            .collect();

        let current_id = self.current_highlight_id();
        let mut new_id = current_id;
        let mut set_pinned_expanded: Option<bool> = None;

        match &key {
            key::Key::Named(key::Named::ArrowUp) if !modifiers.control() => {
                new_id = Self::prev_id(&nav, current_id);
                if show_sections
                    && !self.state.ui.pinned_expanded
                    && new_id.is_some_and(|id| pinned.iter().any(|e| e.entry.id() == id))
                {
                    set_pinned_expanded = Some(true);
                }
            }
            key::Key::Named(key::Named::ArrowDown) if !modifiers.control() => {
                new_id = Self::next_id(&nav, current_id);
                if show_sections
                    && !self.state.ui.pinned_expanded
                    && new_id.is_some_and(|id| pinned.iter().any(|e| e.entry.id() == id))
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
                    return self.paste(id);
                }
                return Task::none();
            }
            key::Key::Named(key::Named::Escape) => {
                if self.state.ui.details_requested.is_some() {
                    return Task::done(Message::DetailClosed);
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
            key::Key::Named(key::Named::Tab) if modifiers.control() => {
                return if modifiers.shift() {
                    Task::done(Message::TabPrev)
                } else {
                    Task::done(Message::TabNext)
                };
            }
            key::Key::Character(c) => {
                let s = c.as_str();

                // Regular typing is intentionally left to the search text input
                // (launcher-style UI). Only handle modifier combinations here.
                if modifiers.control() {
                    if s.eq_ignore_ascii_case("r") {
                        return Task::done(Message::Refresh);
                    }
                    if s.eq_ignore_ascii_case("d") {
                        return self.toggle_detail();
                    }
                    if let Some(digit) = s.chars().next().and_then(|c| c.to_digit(10)) {
                        let idx = digit as usize;
                        if let Some(entry) = nav.get(idx) {
                            return self.paste(entry.entry.id());
                        }
                    }
                    return Task::none();
                }

                if modifiers.alt() {
                    if s.eq_ignore_ascii_case("x") || s.eq_ignore_ascii_case("m") {
                        return self.toggle_search_kind();
                    }
                    if let Some(digit) = s.chars().next().and_then(|c| c.to_digit(10))
                        && (1..=4).contains(&digit)
                    {
                        return Task::done(Message::TabSelected(
                            ActiveTab::ALL[digit as usize - 1],
                        ));
                    }
                    return Task::none();
                }

                return Task::none();
            }
            _ => return Task::none(),
        }

        // Apply navigation changes.
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

    fn request_images(&mut self, entries: &[UiEntry]) {
        for entry in entries {
            if matches!(entry.cache, UiEntryCache::Image) {
                self.request_image(entry.entry.id());
            }
        }
    }

    fn request_image(&mut self, id: u64) {
        if !self.loaded_image_pending.contains(&id) && !self.image_cache.contains_key(&id) {
            let _ = self.requests.send(Command::LoadImage(id));
        }
    }

    fn next_id(nav: &[&UiEntry], current_id: Option<u64>) -> Option<u64> {
        if let Some(id) = current_id {
            let idx = nav.iter().position(|e| e.entry.id() == id);
            if idx == Some(nav.len().saturating_sub(1)) || idx.is_none() {
                nav.first().map(|e| e.entry.id())
            } else {
                idx.and_then(|i| i.checked_add(1))
                    .and_then(|i| nav.get(i))
                    .map(|e| e.entry.id())
            }
        } else {
            nav.first().map(|e| e.entry.id())
        }
    }

    fn prev_id(nav: &[&UiEntry], current_id: Option<u64>) -> Option<u64> {
        if let Some(id) = current_id {
            let idx = nav.iter().position(|e| e.entry.id() == id);
            if idx == Some(0) || idx.is_none() {
                nav.last().map(|e| e.entry.id())
            } else {
                idx.and_then(|i| i.checked_sub(1))
                    .and_then(|i| nav.get(i))
                    .map(|e| e.entry.id())
            }
        } else {
            nav.last().map(|e| e.entry.id())
        }
    }
}
