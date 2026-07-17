use std::{
    env, io,
    sync::{
        Arc, mpsc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender},
    },
    thread,
};

use ::image as image_crate;
use futures::{Stream, SinkExt};
use iced::{
    Element, Event, Subscription, Task,
    event::Status,
    keyboard::{self, key},
    widget::{image, operation},
    window,
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
use crate::utils::{decode_image_async, load_server_config_async, save_server_config_async};

pub type ImageCache = std::collections::HashMap<u64, image::Handle>;
pub type LoadedImagePending = std::collections::HashSet<u64>;

/// The application model plus communication channels (TEA Model).
pub struct RingboardApp {
    pub requests: Sender<Command>,
    pub state: State,
    pub image_cache: ImageCache,
    pub loaded_image_pending: LoadedImagePending,
    /// Whether closing the window should hide it (resuming instantly on the
    /// next launch) instead of exiting the process, mirroring the egui
    /// client's background behavior.
    daemon: bool,
    /// Tells the `maintain_single_instance` background thread to stop when
    /// the app is really exiting (not just hiding).
    stop: Arc<AtomicBool>,
    /// This window's id, resolved once at boot; needed to hide/show/focus it.
    window_id: Option<window::Id>,
}

/// Bridges the background controller thread's blocking `Receiver` into an
/// async stream, so the UI is woken only when a message actually arrives
/// instead of polling on a timer.
fn controller_messages(responses: Receiver<ControllerMessage>) -> impl Stream<Item = Message> {
    iced::stream::channel(8, async move |mut output| {
        thread::spawn(move || {
            while let Ok(msg) = responses.recv() {
                // `send` (not `try_send`) so a momentary burst (e.g. many
                // image loads after the first page loads) applies
                // backpressure instead of erroring out and permanently
                // killing this forwarder thread.
                if futures::executor::block_on(output.send(Message::Controller(Arc::new(msg))))
                    .is_err()
                {
                    break;
                }
            }
        });
    })
}

/// Bridges the `maintain_single_instance` background thread's wake signal
/// into an async stream.
fn wake_messages(rx: Receiver<()>) -> impl Stream<Item = Message> {
    iced::stream::channel(1, async move |mut output| {
        thread::spawn(move || {
            while rx.recv().is_ok() {
                if futures::executor::block_on(output.send(Message::WakeRequested)).is_err() {
                    break;
                }
            }
        });
    })
}

/// `keyboard::listen()` only delivers events the widget tree *ignored*. The
/// always-focused search input captures Left/Right itself (to move its text
/// cursor), so without this they'd never reach `handle_key_event` at all.
/// Only forwards the ones that were actually captured, so nothing is ever
/// delivered twice.
fn captured_arrow_key(event: Event, status: Status, _window: window::Id) -> Option<Message> {
    if status != Status::Captured {
        return None;
    }
    match event {
        Event::Keyboard(
            event @ keyboard::Event::KeyPressed {
                key: key::Key::Named(key::Named::ArrowLeft | key::Named::ArrowRight),
                ..
            },
        ) => Some(Message::KeyEvent(event)),
        _ => None,
    }
}

impl RingboardApp {
    /// Initialize the model and spawn the background controller thread.
    pub fn boot(startup_token: Option<crate::startup::Token>) -> (Self, Task<Message>) {
        let (command_sender, command_receiver) = mpsc::channel();
        let (response_sender, response_receiver) = mpsc::sync_channel(8);
        let requests = command_sender.clone();

        thread::spawn(move || {
            controller(&command_receiver, |m| {
                response_sender.send(m).map_err(|_| ())
            });
        });

        let daemon = env::var_os("RINGBOARD_NO_DAEMON").is_none();
        let stop = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        if daemon {
            let stop = stop.clone();
            thread::spawn(move || {
                if let Err(e) = crate::startup::maintain_single_instance(
                    &stop,
                    startup_token,
                    move || {
                        let _ = wake_tx.send(());
                    },
                ) {
                    eprintln!("Single-instance background thread failed: {e}");
                }
            });
        }

        let state = State::new();
        let app = RingboardApp {
            requests,
            state,
            image_cache: ImageCache::default(),
            loaded_image_pending: LoadedImagePending::default(),
            daemon,
            stop,
            window_id: None,
        };

        let focus_search = operation::focus(crate::widgets::search_input_id());
        let controller_stream = Task::stream(controller_messages(response_receiver));
        let mut tasks = vec![focus_search, controller_stream];
        if daemon {
            tasks.push(Task::stream(wake_messages(wake_rx)));
            tasks.push(window::latest().map(Message::WindowIdResolved));
        }
        (app, Task::batch(tasks))
    }

    pub fn title(&self) -> String {
        format!("Ringboard v{}", env!("CARGO_PKG_VERSION"))
    }

    /// The TEA update function: (Model, Msg) -> (Model, Cmd).
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Controller(msg) => self.handle_incoming_controller_message(msg),
            Message::KeyEvent(event) => self.handle_key_event(event),
            Message::WindowEvent(id, event) => self.handle_window_event(id, event),
            Message::WindowIdResolved(id) => {
                self.window_id = self.window_id.or(id);
                Task::none()
            }
            Message::WakeRequested => self.wake(),
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
            Message::EntryHovered(id) => {
                self.state.ui.hovered_id = id;
                Task::none()
            }
            Message::SettingsLoaded(result) => {
                match result {
                    Ok((main, favorites)) => {
                        self.state.settings.max_main_entries = main.to_string();
                        self.state.settings.max_favorite_entries = favorites.to_string();
                    }
                    Err(e) => self.state.settings.status = Some(Err(e)),
                }
                self.state.settings.loaded = true;
                Task::none()
            }
            Message::SettingsMaxMainChanged(value) => {
                self.state.settings.max_main_entries = value;
                Task::none()
            }
            Message::SettingsMaxFavoritesChanged(value) => {
                self.state.settings.max_favorite_entries = value;
                Task::none()
            }
            Message::SettingsSaveRequested => self.save_settings(),
            Message::SettingsSaved(result) => {
                self.state.settings.saving = false;
                self.state.settings.status = Some(
                    result
                        .map(|()| "Saved. Restart the Ringboard server to apply.".to_string()),
                );
                Task::none()
            }
            Message::SettingsGcBytesChanged(value) => {
                self.state.settings.gc_max_wasted_bytes = value;
                Task::none()
            }
            Message::SettingsGcRequested => self.run_gc(),
        }
    }

    /// The TEA view function: Model -> Html/Element.
    pub fn view(&self) -> Element<'_, Message> {
        crate::widgets::main_view(self)
    }

    /// The TEA subscriptions: Model -> Subscriptions.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            keyboard::listen().map(Message::KeyEvent),
            window::events().map(|(id, event)| Message::WindowEvent(id, event)),
            iced::event::listen_with(captured_arrow_key),
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
        if self.state.ui.active_tab == ActiveTab::Settings {
            return Vec::new();
        }
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
                ActiveTab::Settings => unreachable!(),
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

    fn handle_window_event(&mut self, id: window::Id, event: window::Event) -> Task<Message> {
        self.window_id.get_or_insert(id);
        match event {
            window::Event::Focused => operation::focus(crate::widgets::search_input_id()),
            window::Event::CloseRequested => {
                if self.daemon {
                    self.hide_window(id)
                } else {
                    self.exit()
                }
            }
            _ => Task::none(),
        }
    }

    /// Hides the window instead of exiting, if running as a background
    /// daemon; otherwise exits the process for real. Used for Escape and
    /// after a successful paste (the window's native close button is
    /// handled directly in `handle_window_event`, using the id the
    /// `CloseRequested` event itself carries).
    fn close_or_hide(&mut self) -> Task<Message> {
        let Some(id) = self.window_id.filter(|_| self.daemon) else {
            return self.exit();
        };
        self.hide_window(id)
    }

    fn hide_window(&mut self, id: window::Id) -> Task<Message> {
        self.state.reset();
        self.image_cache.clear();
        self.loaded_image_pending.clear();
        // Minimizing is respected far more consistently across window
        // managers than `Mode::Hidden` (e.g. GNOME/mutter's client-side
        // decoration frame doesn't reliably follow `set_visible(false)`).
        window::minimize(id, true)
    }

    fn exit(&mut self) -> Task<Message> {
        self.stop.store(true, Ordering::Relaxed);
        crate::startup::cleanup();
        std::process::exit(0);
    }

    /// Called when another `toggle` invocation asked us to wake up.
    fn wake(&mut self) -> Task<Message> {
        let Some(id) = self.window_id else {
            return Task::none();
        };
        Task::batch([
            window::minimize(id, false),
            window::gain_focus(id),
            operation::focus(crate::widgets::search_input_id()),
            self.refresh_entries(),
        ])
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
        if tab == ActiveTab::Settings && !self.state.settings.loaded {
            return Task::perform(load_server_config_async(), Message::SettingsLoaded);
        }
        Task::none()
    }

    fn save_settings(&mut self) -> Task<Message> {
        let Ok(max_main) = self.state.settings.max_main_entries.trim().parse() else {
            self.state.settings.status =
                Some(Err("Max main entries must be a positive number".into()));
            return Task::none();
        };
        let Ok(max_favorites) = self.state.settings.max_favorite_entries.trim().parse() else {
            self.state.settings.status =
                Some(Err("Max favorite entries must be a positive number".into()));
            return Task::none();
        };

        self.state.settings.saving = true;
        self.state.settings.status = None;
        Task::perform(
            save_server_config_async(max_main, max_favorites),
            Message::SettingsSaved,
        )
    }

    fn run_gc(&mut self) -> Task<Message> {
        let Ok(max_wasted_bytes) = self.state.settings.gc_max_wasted_bytes.trim().parse() else {
            self.state.settings.status = Some(Err(
                "Max wasted bytes must be a non-negative number".into()
            ));
            return Task::none();
        };

        self.state.settings.running_gc = true;
        self.state.settings.status = None;
        let _ = self
            .requests
            .send(Command::GarbageCollect { max_wasted_bytes });
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

    fn handle_incoming_controller_message(&mut self, msg: Arc<ControllerMessage>) -> Task<Message> {
        let Some(msg) = Arc::into_inner(msg) else {
            return Task::none();
        };
        if let ControllerMessage::LoadedImage { id, image } = msg {
            if !self.loaded_image_pending.contains(&id) {
                self.loaded_image_pending.insert(id);
                return Task::perform(decode_image_async(id, image), |(id, result)| {
                    Message::ImageDecoded(id, result)
                });
            }
            return Task::none();
        }
        self.handle_controller_message(msg)
    }

    fn handle_controller_message(&mut self, msg: ControllerMessage) -> Task<Message> {
        match msg {
            ControllerMessage::FatalDbOpen(e) => {
                self.state.ui.fatal_error = Some(ClientError::Core(e));
                Task::none()
            }
            ControllerMessage::Error(e) => {
                self.state.settings.running_gc = false;
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
            ControllerMessage::Pasted => self.close_or_hide(),
            ControllerMessage::GarbageCollected { bytes_freed } => {
                self.state.settings.running_gc = false;
                self.state.settings.status =
                    Some(Ok(format!("Freed {bytes_freed} bytes.")));
                self.refresh_entries()
            }
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
                let on_pinned_entry =
                    current_id.is_some_and(|id| pinned.iter().any(|e| e.entry.id() == id));
                if show_sections && !pinned.is_empty() && on_pinned_entry {
                    set_pinned_expanded = Some(false);
                    new_id = unpinned.first().map(|e| e.entry.id());
                } else if let Some(id) = current_id
                    && self.state.ui.details_requested == Some(id)
                {
                    return Task::done(Message::DetailClosed);
                }
            }
            key::Key::Named(key::Named::ArrowRight) => {
                let on_collapsed_pinned_entry = !self.state.ui.pinned_expanded
                    && current_id.is_some_and(|id| pinned.iter().any(|e| e.entry.id() == id));
                if show_sections && !pinned.is_empty() && on_collapsed_pinned_entry {
                    set_pinned_expanded = Some(true);
                } else if let Some(id) = current_id
                    && self.state.ui.details_requested != Some(id)
                    && self.entry_has_extra_detail(id)
                {
                    return Task::done(Message::DetailRequested(id));
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
                return self.close_or_hide();
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
                        && (1..=ActiveTab::ALL.len() as u32).contains(&digit)
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

        if new_id.is_some() && new_id != current_id {
            self.scroll_to_highlighted()
        } else {
            Task::none()
        }
    }

    fn entry_has_extra_detail(&self, id: u64) -> bool {
        self.state
            .entries
            .loaded_entries
            .iter()
            .chain(self.state.entries.search_results.iter())
            .find(|e| e.entry.id() == id)
            .is_some_and(crate::widgets::entry_has_extra_detail)
    }

    /// Keeps the highlighted entry roughly in view after keyboard
    /// navigation. Approximate (based on row index, not pixel position)
    /// since row heights vary (text vs. image previews).
    fn scroll_to_highlighted(&self) -> Task<Message> {
        let show_sections =
            self.state.ui.query.is_empty() && self.state.ui.active_tab == ActiveTab::All;
        let filtered = self.filtered_entries();
        let render_order: Vec<u64> = if show_sections {
            let pinned = filtered
                .iter()
                .filter(|e| e.entry.ring() == RingKind::Favorites);
            let unpinned = filtered
                .iter()
                .filter(|e| e.entry.ring() == RingKind::Main);
            if self.state.ui.pinned_expanded {
                pinned.chain(unpinned).map(|e| e.entry.id()).collect()
            } else {
                unpinned.map(|e| e.entry.id()).collect()
            }
        } else {
            filtered.iter().map(|e| e.entry.id()).collect()
        };

        let Some(current_id) = self.current_highlight_id() else {
            return Task::none();
        };
        let Some(idx) = render_order.iter().position(|&id| id == current_id) else {
            return Task::none();
        };
        let fraction = if render_order.len() <= 1 {
            0.0
        } else {
            idx as f32 / (render_order.len() - 1) as f32
        };

        operation::snap_to(
            crate::widgets::entry_list_id(),
            operation::RelativeOffset { x: 0.0, y: fraction },
        )
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
