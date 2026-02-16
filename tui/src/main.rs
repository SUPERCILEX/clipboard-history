#![feature(default_field_values)]

use std::{
    fmt::Write,
    fs::File,
    io,
    io::{BufReader, BufWriter},
    mem::ManuallyDrop,
    os::fd::FromRawFd,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender},
    },
    thread,
};

use error_stack::Report;
use image::{DynamicImage, ImageReader};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
    buffer::Buffer,
    crossterm::{
        ExecutableCommand, event,
        event::{Event, KeyEvent, KeyEventKind, KeyModifiers},
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, HighlightSpacing, List, ListState, Padding, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget, Wrap,
    },
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};
use ringboard_sdk::{
    core::{Error as CoreError, IoErr, protocol::RingKind},
    search::{CancellationTokenSink, cancellation_token},
    ui_actor::{
        Command, CommandError, DetailedEntry, Message, SearchKind, UiEntry, UiEntryCache,
        controller,
    },
};
use rustix::{
    process::{getpriority_process, setpriority_process},
    stdio::raw_stdout,
};
use thiserror::Error;
use tui_input::{Input, backend::crossterm::EventHandler};

#[cfg(feature = "trace")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

enum Action {
    Controller(Message),
    User(io::Result<Event>),
    ImageLoaded { id: u64, image: DynamicImage },
}

impl From<Message> for Action {
    fn from(value: Message) -> Self {
        Self::Controller(value)
    }
}

impl From<io::Result<Event>> for Action {
    fn from(value: io::Result<Event>) -> Self {
        Self::User(value)
    }
}

struct App {
    requests: Sender<Command>,
    responses: Receiver<Action>,
    picker: Picker,
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

    loaded_state: ListState,
    search_state: ListState,
}

#[derive(Default)]
struct UiState {
    last_error: Option<CommandError>,
    outstanding_request: Option<u64>,

    details_requested: Option<u64>,
    detailed_entry: Option<Result<DetailedEntry, CoreError>>,
    detail_scroll: Option<ScrollbarState>,
    detail_image_state: Option<ImageState>,

    query: Input,
    search_state: Option<SearchState> = Some(SearchState {
        focused: true,
        kind: SearchKind::Plain,
    }),
    pending_search_token: Option<CancellationTokenSink>,

    show_help: bool,

    cache: String,
}

struct SearchState {
    focused: bool,
    kind: SearchKind,
}

#[allow(clippy::large_enum_variant)]
enum ImageState {
    Requested(u64),
    Loaded(StatefulProtocol),
}

macro_rules! active_entries {
    ($entries:expr, $state:expr) => {{
        if $state.query.value().is_empty() {
            &$entries.loaded_entries
        } else {
            &$entries.search_results
        }
    }};
}

macro_rules! active_list_state {
    ($entries:expr, $state:expr) => {{
        if $state.query.value().is_empty() {
            &mut $entries.loaded_state
        } else {
            &mut $entries.search_state
        }
    }};
}

macro_rules! selected_entry {
    ($entries:expr, $state:expr) => {{
        if $state.query.value().is_empty() {
            &$entries.loaded_state
        } else {
            &$entries.search_state
        }
        .selected()
        .and_then(|selected| active_entries!($entries, $state).get(selected))
    }};
}

#[derive(Error, Debug)]
enum Wrapper {
    #[error("{0}")]
    W(String),
}

fn main() -> Result<(), Report<Wrapper>> {
    #[cfg(not(debug_assertions))]
    error_stack::Report::install_debug_hook::<std::panic::Location>(|_, _| {});

    run().map_err(|e| {
        let wrapper = Wrapper::W(e.to_string());
        e.into_report(wrapper)
    })
}

fn run() -> Result<(), CoreError> {
    let stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(raw_stdout()) });
    let mut stdout = BufWriter::new(&*stdout);

    let mut terminal = init_terminal(&mut stdout)?;
    let r = App::init(&mut terminal).and_then(|app| app.run(terminal));
    restore_terminal(&mut stdout)?;
    r
}

fn init_terminal(
    mut stdout: impl io::Write,
) -> Result<Terminal<impl Backend<Error = io::Error>>, CoreError> {
    std::panic::set_hook({
        let hook = std::panic::take_hook();
        Box::new(move |info| {
            if let Err(err) = restore_terminal(io::stdout()) {
                eprintln!("Failed to restore terminal: {err}");
            }
            hook(info);
        })
    });

    enable_raw_mode().map_io_err(|| "Failed to enable raw mode.")?;
    stdout
        .execute(EnterAlternateScreen)
        .map_io_err(|| "Failed to enter alternate screen.")?;
    Terminal::new(CrosstermBackend::new(stdout)).map_io_err(|| "Failed to initialize terminal.")
}

fn restore_terminal(mut stdout: impl io::Write) -> Result<(), CoreError> {
    disable_raw_mode().map_io_err(|| "Failed to disable raw mode.")?;
    stdout
        .execute(LeaveAlternateScreen)
        .map_io_err(|| "Failed to leave alternate screen.")?;
    Ok(())
}

impl App {
    fn init(terminal: &mut Terminal<impl Backend<Error = io::Error>>) -> Result<Self, CoreError> {
        let (command_sender, command_receiver) = mpsc::channel();
        let (response_sender, response_receiver) = mpsc::sync_channel(8);
        let mut state = State::default();

        AppWrapper {
            state: &mut state,
            requests: &command_sender,
            cursor_position: None,
        }
        .draw(terminal)
        .map_io_err(|| "Failed to write to terminal.")?;

        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());

        thread::spawn({
            let sender = response_sender.clone();
            move || {
                controller(&command_receiver, |m| {
                    if let Message::LoadedImage { id, image } = m {
                        let sender = sender.clone();
                        thread::spawn(move || {
                            let run = || {
                                let priority = getpriority_process(None)
                                    .map_io_err(|| "Failed to get image loading thread priority")?;
                                let priority = priority + 1;
                                setpriority_process(None, priority).map_io_err(|| {
                                    format!(
                                        "Failed to lower image loading thread priority to \
                                         {priority}."
                                    )
                                })?;
                                Ok(ImageReader::new(BufReader::new(image))
                                    .with_guessed_format()
                                    .map_io_err(|| {
                                        format!("Failed to guess image format for entry {id}.")
                                    })?
                                    .decode()?)
                            };
                            let _ = match run() {
                                Ok(image) => sender.send(Action::ImageLoaded { id, image }),
                                Err(e) => sender.send(Message::Error(e).into()),
                            };
                        });
                        Ok(())
                    } else {
                        sender.send(m.into())
                    }
                });
            }
        });
        thread::spawn(move || {
            loop {
                let r = event::read();
                let oopsies = r.is_err();
                if response_sender.send(r.into()).is_err() || oopsies {
                    break;
                }
            }
        });

        Ok(Self {
            requests: command_sender,
            responses: response_receiver,
            picker,

            state,
        })
    }
}

impl App {
    fn run(
        mut self,
        mut terminal: Terminal<impl Backend<Error = io::Error>>,
    ) -> Result<(), CoreError> {
        let Self {
            requests,
            responses,
            ref picker,
            ref mut state,
        } = self;

        let mut local_state = Option::default();
        for action in responses {
            if match action {
                Action::Controller(message) => {
                    handle_message(message, state, &mut local_state, &requests)?
                }
                Action::ImageLoaded { id, image } => {
                    if let Some(ImageState::Requested(requested_id)) = state.ui.detail_image_state
                        && requested_id == id
                    {
                        state.ui.detail_image_state =
                            Some(ImageState::Loaded(picker.new_resize_protocol(image)));
                    }
                    false
                }
                Action::User(event) => handle_event(
                    &event.map_io_err(|| "Failed to read terminal.")?,
                    state,
                    &requests,
                ),
            } {
                break;
            }

            AppWrapper {
                state,
                requests: &requests,
                cursor_position: None,
            }
            .draw(&mut terminal)
            .map_io_err(|| "Failed to write to terminal.")?;
        }
        Ok(())
    }
}

fn maybe_focus_pending_changed_entry(
    entries: &mut UiEntries,
    ui: &mut UiState,
    pending_favorite_change: &mut Option<u64>,
) {
    if let Some(id) = *pending_favorite_change
        && let Some(index) = active_entries!(entries, ui)
            .iter()
            .position(|e| e.entry.id() == id)
    {
        pending_favorite_change.take();
        active_list_state!(entries, ui).select(Some(index));
        if ui.details_requested.is_some() {
            ui.details_requested = Some(id);
        }
    }
}

fn handle_message(
    message: Message,
    State { entries, ui }: &mut State,
    pending_favorite_change: &mut Option<u64>,
    requests: &Sender<Command>,
) -> Result<bool, CoreError> {
    let UiEntries {
        loaded_entries,
        search_results,
        loaded_state,
        search_state,
    } = entries;
    let UiState {
        details_requested,
        detailed_entry,
        pending_search_token,
        last_error,
        outstanding_request,
        ..
    } = ui;

    last_error.take();
    match message {
        Message::FatalDbOpen(e) => return Err(e)?,
        Message::Error(e) => {
            *last_error = Some(e);
            pending_search_token.take_if(|token| token.is_done());
        }
        Message::LoadedFirstPage {
            entries: new_entries,
            default_focused_id,
        } => {
            *loaded_entries = new_entries;
            if loaded_state.selected().is_none() {
                loaded_state.select(default_focused_id.and_then(|selected_id| {
                    loaded_entries
                        .iter()
                        .position(|e| e.entry.id() == selected_id)
                }));
                maybe_get_details(entries, ui, requests);
            }
            maybe_focus_pending_changed_entry(entries, ui, pending_favorite_change);
        }
        Message::EntryDetails { id, result } => {
            if *details_requested == Some(id) {
                *detailed_entry = Some(result);
            }
        }
        Message::SearchResults(results) => {
            pending_search_token.take_if(|token| token.is_done());
            *search_results = results;
            if search_state.selected().is_none() {
                search_state.select_first();
            }
            maybe_focus_pending_changed_entry(entries, ui, pending_favorite_change);
        }
        Message::FavoriteChange(id) => {
            *pending_favorite_change = Some(id);
            outstanding_request.take_if(|&mut req_id| req_id == id);
        }
        Message::Deleted(id) => {
            outstanding_request.take_if(|&mut req_id| req_id == id);
        }
        Message::LoadedImage { .. } => unreachable!(),
        Message::Pasted => return Ok(true),
    }
    if ui.details_requested.is_some() {
        maybe_get_details(entries, ui, requests);
    }
    Ok(false)
}

fn maybe_get_details(entries: &UiEntries, ui: &mut UiState, requests: &Sender<Command>) {
    if let Some(&UiEntry { entry, ref cache }) = selected_entry!(entries, ui)
        && ui.details_requested != Some(entry.id())
    {
        ui.details_requested = Some(entry.id());
        ui.detailed_entry = None;
        ui.detail_scroll = None;
        ui.detail_image_state = None;
        let _ = requests.send(Command::GetDetails {
            id: entry.id(),
            with_text: cache.is_text(),
        });
    }
}

fn handle_event(event: &Event, state: &mut State, requests: &Sender<Command>) -> bool {
    let State { entries, ui } = state;

    let unselect = |ui: &mut UiState| {
        ui.details_requested = None;
        ui.detailed_entry = None;
    };
    let search = |ui: &mut UiState, kind: SearchKind| {
        if ui.query.value().is_empty() {
            return;
        }

        let (source, sink) = cancellation_token();
        let _ = requests.send(Command::Search {
            query: ui.query.value().into(),
            kind,
            token: source,
        });
        ui.pending_search_token = Some(sink);
    };
    let refresh = |ui: &mut UiState| {
        let _ = requests.send(Command::LoadFirstPage);
        if let &Some(SearchState { focused: _, kind }) = &ui.search_state {
            search(ui, kind);
        }
    };

    match *event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            state: _,
        }) => {
            if kind == KeyEventKind::Press {
                use ratatui::crossterm::event::KeyCode::{Char, Down, Enter, Esc, Left, Right, Up};
                match code {
                    Esc => {
                        if let Some(SearchState { focused, .. }) = &mut ui.search_state
                            && *focused
                        {
                            *focused = false;
                        } else if ui.details_requested.is_some() {
                            unselect(ui);
                        } else if ui.search_state.is_some() {
                            ui.search_state = None;
                            ui.query = Input::default();
                        } else {
                            return true;
                        }
                    }
                    Enter => {
                        if let Some(SearchState { focused, .. }) = &mut ui.search_state
                            && *focused
                        {
                            *focused = false;
                        } else if let Some(&UiEntry { entry, cache: _ }) =
                            selected_entry!(entries, ui)
                        {
                            let _ = requests.send(Command::Paste(entry.id()));
                        }
                    }
                    _ => {}
                }

                if let &mut Some(SearchState {
                    ref mut focused,
                    kind,
                }) = &mut ui.search_state
                    && *focused
                {
                    let changed = ui
                        .query
                        .handle_event(event)
                        .is_some_and(|changed| changed.value);
                    if changed {
                        search(ui, kind);
                    } else if code == Up || code == Down {
                        *focused = false;
                    }
                } else {
                    match code {
                        Char('q') => return true,
                        Char('c') if modifiers == KeyModifiers::CONTROL => return true,
                        Char(c @ '0'..='9') => {
                            if let Some(UiEntry { entry, cache: _ }) = active_entries!(entries, ui)
                                .get(usize::try_from(u32::from(c) - u32::from('0')).unwrap())
                            {
                                let _ = requests.send(Command::Paste(entry.id()));
                            }
                        }
                        Char('h') | Left => unselect(ui),
                        Char('j') | Down => {
                            let state = active_list_state!(entries, ui);
                            let len = active_entries!(entries, ui).len();
                            let next = state
                                .selected()
                                .map_or(0, |i| if i + 1 == len { 0 } else { i + 1 });
                            state.select(Some(next.min(len)));
                        }
                        Char('J') => {
                            if let Some(detail_scroll) = &mut ui.detail_scroll {
                                detail_scroll.next();
                            }
                        }
                        Char('k') | Up => {
                            let state = active_list_state!(entries, ui);
                            let len = active_entries!(entries, ui).len();
                            let previous = state.selected().map_or(usize::MAX, |i| {
                                if i == 0 { len.wrapping_sub(1) } else { i - 1 }
                            });
                            if let Some(SearchState { focused, .. }) = &mut ui.search_state
                                && Some(previous) > state.selected()
                            {
                                *focused = true;
                            } else {
                                state.select(Some(previous.min(len)));
                            }
                        }
                        Char('K') => {
                            if let Some(detail_scroll) = &mut ui.detail_scroll {
                                detail_scroll.prev();
                            }
                        }
                        Char('l') | Right => maybe_get_details(entries, ui, requests),
                        Char(' ') => {
                            if ui.details_requested.is_some() {
                                unselect(ui);
                            } else {
                                maybe_get_details(entries, ui, requests);
                            }
                        }
                        Char(c @ ('/' | 's' | 'x' | 'm')) => {
                            let kind = match c {
                                'x' => SearchKind::Regex,
                                'm' => SearchKind::Mime,
                                _ => SearchKind::Plain,
                            };
                            ui.search_state = Some(SearchState {
                                focused: true,
                                kind,
                            });
                            search(ui, kind);
                        }
                        Char('f') => {
                            if let Some(&UiEntry { entry, cache: _ }) = selected_entry!(entries, ui)
                                && ui.outstanding_request != Some(entry.id())
                            {
                                ui.outstanding_request = Some(entry.id());
                                match entry.ring() {
                                    RingKind::Favorites => {
                                        let _ = requests.send(Command::Unfavorite(entry.id()));
                                    }
                                    RingKind::Main => {
                                        let _ = requests.send(Command::Favorite(entry.id()));
                                    }
                                }
                                refresh(ui);
                            }
                        }
                        Char('d') => {
                            if let Some(&UiEntry { entry, cache: _ }) = selected_entry!(entries, ui)
                                && ui.outstanding_request != Some(entry.id())
                            {
                                ui.outstanding_request = Some(entry.id());
                                let _ = requests.send(Command::Delete(entry.id()));
                                refresh(ui);
                            }
                        }
                        Char('?') => {
                            ui.show_help ^= true;
                        }
                        Char('r') => {
                            if modifiers == KeyModifiers::CONTROL {
                                *state = State::default();
                            }
                            refresh(&mut state.ui);
                            return false;
                        }
                        _ => {}
                    }
                }
            }
        }
        Event::FocusGained => {
            refresh(ui);
        }
        _ => {}
    }
    if ui.details_requested.is_some() {
        maybe_get_details(entries, ui, requests);
    }
    false
}

struct AppWrapper<'a> {
    requests: &'a Sender<Command>,
    state: &'a mut State,
    cursor_position: Option<Position>,
}

impl AppWrapper<'_> {
    fn draw(&mut self, terminal: &mut Terminal<impl Backend<Error = io::Error>>) -> io::Result<()> {
        terminal.draw(|f| {
            f.render_widget(&mut *self, f.area());
            if let Some(cursor_position) = self.cursor_position {
                f.set_cursor_position(cursor_position);
            }
        })?;
        Ok(())
    }
}

impl Widget for &mut AppWrapper<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let State { entries: _, ui } = &self.state;
        let has_error = ui.last_error.is_some();

        let [header_area, main_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(if ui.show_help { 3 } else { 0 }),
        ])
        .areas(area);

        let [entry_list_area, _padding, selected_entry_area] =
            if ui.details_requested.is_none() && !has_error {
                Layout::vertical([
                    Constraint::Min(0),
                    Constraint::Length(0),
                    Constraint::Length(0),
                ])
            } else if area.width <= area.height * 3 {
                Layout::vertical([
                    Constraint::Percentage(50),
                    Constraint::Length(0),
                    Constraint::Percentage(50),
                ])
            } else {
                Layout::horizontal([
                    Constraint::Percentage(50),
                    Constraint::Length(1),
                    Constraint::Percentage(50),
                ])
            }
            .areas(main_area);

        AppWrapper::render_title(header_area, buf);
        self.render_entries(entry_list_area, buf);
        if has_error {
            self.render_error(selected_entry_area, buf);
        } else {
            self.render_selected_entry(selected_entry_area, buf);
        }
        AppWrapper::render_footer(footer_area, buf);
    }
}

fn ui_entry_line(UiEntry { entry: _, cache }: &UiEntry) -> Line<'_> {
    match cache {
        &UiEntryCache::HighlightedText {
            ref one_liner,
            start,
            end,
        } => Line::default().spans([
            Span::raw(&one_liner[..start]),
            Span::styled(&one_liner[start..end], Modifier::UNDERLINED),
            Span::raw(&one_liner[end..]),
        ]),
        UiEntryCache::Text { one_liner } => Line::raw(&**one_liner),
        UiEntryCache::Image => Line::raw("Image: open details to view.").italic(),
        UiEntryCache::Binary { mime_type } => {
            Line::raw(format!("Unable to display format of type {mime_type:?}.")).italic()
        }
        UiEntryCache::Error(e) => Line::raw(format!("Error: {e}\nDetails: {e:#?}")).italic(),
    }
}

impl AppWrapper<'_> {
    fn render_entries(&mut self, area: Rect, buf: &mut Buffer) {
        let Self {
            state: State { entries, ui },
            requests: _,
            cursor_position,
        } = self;

        let [search_area, entries_area] = Layout::vertical([
            Constraint::Length(if ui.search_state.is_some() { 3 } else { 0 }),
            Constraint::Min(0),
        ])
        .areas(area);

        if let &Some(SearchState { focused, kind }) = &ui.search_state {
            let search_input = Block::default()
                .borders(Borders::ALL)
                .border_style(if focused {
                    Style::new().bold()
                } else {
                    Style::default()
                })
                .title(if ui.pending_search_token.is_some() {
                    "Searching…"
                } else {
                    match kind {
                        SearchKind::Plain => "Search",
                        SearchKind::Regex => "RegEx search",
                        SearchKind::Mime => "Mime type search",
                    }
                });

            let y_scroll = ui
                .query
                .visual_scroll(usize::from(search_area.width.max(3) - 3));
            let search_input = Paragraph::new(ui.query.value())
                .scroll((0, u16::try_from(y_scroll).unwrap_or(u16::MAX)))
                .block(search_input);

            search_input.render(search_area, buf);
            *cursor_position = Some(
                (
                    u16::try_from(
                        usize::from(search_area.x) + ui.query.visual_cursor().max(y_scroll)
                            - y_scroll
                            + 1,
                    )
                    .unwrap_or(u16::MAX),
                    search_area.y + 1,
                )
                    .into(),
            );
        }

        let outer_block = Block::new()
            .title_alignment(Alignment::Center)
            .borders(Borders::TOP)
            .title("Entries");
        let inner_block = Block::new().borders(Borders::NONE);
        let inner_area = outer_block.inner(entries_area);

        outer_block.render(entries_area, buf);

        if active_entries!(entries, ui).is_empty() {
            Line::raw("Nothing to see here")
                .italic()
                .render(inner_area, buf);
        } else {
            StatefulWidget::render(
                List::new(active_entries!(entries, ui).iter().map(ui_entry_line))
                    .block(inner_block)
                    .highlight_style(
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .add_modifier(Modifier::REVERSED),
                    )
                    .highlight_spacing(HighlightSpacing::Always),
                inner_area,
                buf,
                active_list_state!(entries, ui),
            );
        }
    }

    fn render_selected_entry(&mut self, area: Rect, buf: &mut Buffer) {
        let Self {
            state: State { entries, ui },
            requests,
            cursor_position: _,
        } = self;
        if area.is_empty() {
            return;
        }
        let Some(UiEntry { entry, cache }) = selected_entry!(entries, ui) else {
            return;
        };

        let outer_block = {
            let mime_type = ui
                .detailed_entry
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .map_or("", |d| &*d.mime_type);

            Block::new()
                .borders(Borders::TOP)
                .title_alignment(Alignment::Center)
                .title({
                    ui.cache.clear();
                    write!(
                        ui.cache,
                        "{} ({}",
                        match entry.ring() {
                            RingKind::Favorites => "Favorite entry",
                            RingKind::Main => "Entry",
                        },
                        entry.id()
                    )
                    .unwrap();
                    if mime_type.is_empty() {
                        write!(ui.cache, ")")
                    } else {
                        write!(ui.cache, "; {mime_type})")
                    }
                    .unwrap();
                    ui.cache.as_str()
                })
        };
        let inner_block = Block::new()
            .borders(Borders::NONE)
            .padding(Padding::horizontal(1));
        let inner_area = outer_block.inner(area);

        outer_block.render(area, buf);

        let error = ui
            .detailed_entry
            .as_ref()
            .and_then(|r| r.as_ref().err())
            .map_or(String::new(), |e| format!("Error: {e}\nDetails: {e:#?}"));

        if matches!(cache, UiEntryCache::Image) {
            if let Some(ImageState::Loaded(image_state)) = &mut ui.detail_image_state {
                StatefulImage::default().render(inner_area, buf, image_state);
            } else {
                Paragraph::new("Loading…")
                    .block(inner_block)
                    .render(inner_area, buf);
            }
            if ui.detail_image_state.is_none() {
                ui.detail_image_state = Some(ImageState::Requested(entry.id()));
                let _ = requests.send(Command::LoadImage(entry.id()));
            }
        } else {
            let paragraph =
                Paragraph::new(ui.detailed_entry.as_ref().map_or("Loading…", |r| match r {
                    Ok(DetailedEntry {
                        mime_type: _,
                        full_text,
                    }) => full_text.as_deref().unwrap_or("Binary data."),
                    Err(_) => &error,
                }))
                .block(inner_block)
                .wrap(Wrap { trim: false });

            {
                let total_lines = paragraph.line_count(inner_area.width.saturating_sub(2));
                let scrollable_lines = total_lines.saturating_sub(usize::from(inner_area.height));

                if scrollable_lines > 0 {
                    let cl = scrollable_lines + 1;
                    if let Some(detail_scroll) = ui.detail_scroll {
                        ui.detail_scroll = Some(
                            detail_scroll
                                .content_length(cl)
                                .position(detail_scroll.get_position().min(scrollable_lines)),
                        );
                    } else {
                        ui.detail_scroll = Some(ScrollbarState::new(cl));
                    }
                } else {
                    ui.detail_scroll = None;
                }
            }

            if let Some(detail_scroll) = &mut ui.detail_scroll {
                paragraph
                    .scroll((
                        u16::try_from(detail_scroll.get_position()).unwrap_or(u16::MAX),
                        0,
                    ))
                    .render(inner_area, buf);

                Scrollbar::new(ScrollbarOrientation::VerticalRight).render(
                    inner_area,
                    buf,
                    detail_scroll,
                );
            } else {
                paragraph.render(inner_area, buf);
            }
        }
    }

    fn render_title(area: Rect, buf: &mut Buffer) {
        Paragraph::new(concat!("Ringboard v", env!("CARGO_PKG_VERSION")))
            .bold()
            .centered()
            .render(area, buf);
    }

    fn render_error(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let Some(error) = &self.state.ui.last_error else {
            return;
        };

        let outer_block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().bold())
            .title_alignment(Alignment::Center)
            .title(format!("Error: {error}"));
        let inner_block = Block::new().borders(Borders::NONE);
        let inner_area = outer_block.inner(area);

        outer_block.render(area, buf);

        Paragraph::new(format!("{error:#?}"))
            .wrap(Wrap { trim: false })
            .block(inner_block)
            .render(inner_area, buf);
    }

    fn render_footer(area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let outer_block = Block::new()
            .borders(Borders::TOP)
            .title_alignment(Alignment::Center)
            .title("Help");
        let inner_block = Block::new().borders(Borders::NONE);
        let inner_area = outer_block.inner(area);

        outer_block.render(area, buf);

        Paragraph::new(
            "Use ↓↑ to move, ←→ to (un)select, / to search, x to search with RegEx, m to search \
             mime types, r to reload, f to (un)favorite, d to delete, J/K to scroll entry details.",
        )
        .wrap(Wrap { trim: true })
        .block(inner_block)
        .centered()
        .render(inner_area, buf);
    }
}
