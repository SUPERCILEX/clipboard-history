#![feature(let_chains)]

use std::{
    io,
    io::stdout,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender},
    },
    thread,
};

use error_stack::Report;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    buffer::Buffer,
    crossterm::{
        event,
        event::{Event, KeyEvent, KeyEventKind, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        ExecutableCommand,
    },
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::Line,
    widgets::{
        Block, Borders, HighlightSpacing, List, ListState, Padding, Paragraph, StatefulWidget,
        Widget, Wrap,
    },
    Terminal,
};
use ringboard_sdk::{
    core::{
        protocol::{IdNotFoundError, RingKind},
        Error as CoreError, IoErr,
    },
    ui_actor::{controller, Command, CommandError, DetailedEntry, Message, UiEntry, UiEntryCache},
    ClientError,
};
use thiserror::Error;

enum Action {
    Controller(Message),
    User(io::Result<Event>),
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
    details_requested: Option<u64>,
    detailed_entry: Option<Result<DetailedEntry, CoreError>>,
    detail_scroll: u16,

    query: String,

    show_help: bool,
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

macro_rules! active_list_state {
    ($entries:expr, $state:expr) => {{
        if $state.query.is_empty() {
            &mut $entries.loaded_state
        } else {
            &mut $entries.search_state
        }
    }};
}

macro_rules! selected_entry {
    ($entries:expr, $state:expr) => {{
        active_list_state!($entries, $state)
            .selected()
            .and_then(|selected| active_entries!($entries, $state).get(selected))
    }};
}

#[derive(Error, Debug)]
enum Wrapper {
    #[error("{0}")]
    W(String),
}

fn main() -> error_stack::Result<(), Wrapper> {
    #[cfg(not(debug_assertions))]
    error_stack::Report::install_debug_hook::<std::panic::Location>(|_, _| {});

    run().map_err(|e| {
        let wrapper = Wrapper::W(e.to_string());
        match e {
            CommandError::Core(e) | CommandError::Sdk(ClientError::Core(e)) => match e {
                CoreError::Io { error, context } => Report::new(error)
                    .attach_printable(context)
                    .change_context(wrapper),
                CoreError::InvalidPidError { error, context } => Report::new(error)
                    .attach_printable(context)
                    .change_context(wrapper),
                CoreError::IdNotFound(IdNotFoundError::Ring(id)) => {
                    Report::new(wrapper).attach_printable(format!("Unknown ring: {id}"))
                }
                CoreError::IdNotFound(IdNotFoundError::Entry(id)) => {
                    Report::new(wrapper).attach_printable(format!("Unknown entry: {id}"))
                }
            },
            CommandError::Sdk(ClientError::InvalidResponse { context }) => {
                Report::new(wrapper).attach_printable(context)
            }
            CommandError::Sdk(ClientError::VersionMismatch { actual: _ }) => Report::new(wrapper),
            CommandError::Regex(e) => Report::new(e).change_context(wrapper),
        }
    })
}

fn run() -> Result<(), CommandError> {
    let terminal = init_terminal()?;
    let r = App::new().run(terminal);
    restore_terminal()?;
    r
}

fn init_terminal() -> Result<Terminal<impl Backend>, CommandError> {
    enable_raw_mode().map_io_err(|| "Failed to enable raw mode.")?;
    stdout()
        .execute(EnterAlternateScreen)
        .map_io_err(|| "Failed to enter alternate screen.")?;
    Ok(Terminal::new(CrosstermBackend::new(stdout().lock()))
        .map_io_err(|| "Failed to initialize terminal.")?)
}

fn restore_terminal() -> Result<(), CommandError> {
    disable_raw_mode().map_io_err(|| "Failed to disable raw mode.")?;
    stdout()
        .execute(LeaveAlternateScreen)
        .map_io_err(|| "Failed to leave alternate screen.")?;
    Ok(())
}

impl App {
    fn new() -> Self {
        let (command_sender, command_receiver) = mpsc::channel();
        let (response_sender, response_receiver) = mpsc::sync_channel(8);
        thread::spawn({
            let sender = response_sender.clone();
            move || controller(&command_receiver, |m| sender.send(m.into()))
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

        Self {
            requests: command_sender,
            responses: response_receiver,

            state: State::default(),
        }
    }
}

impl App {
    fn run(mut self, mut terminal: Terminal<impl Backend>) -> Result<(), CommandError> {
        let Self {
            requests,
            responses,
            ref mut state,
        } = self;
        state
            .draw(&mut terminal)
            .map_io_err(|| "Failed to write to terminal.")?;
        let mut local_state = Option::default();
        for action in responses {
            match action {
                Action::Controller(message) => handle_message(message, state, &mut local_state)?,
                Action::User(event) => {
                    if handle_event(
                        &event.map_io_err(|| "Failed to read terminal.")?,
                        state,
                        &requests,
                    ) {
                        break;
                    }
                }
            }
            state
                .draw(&mut terminal)
                .map_io_err(|| "Failed to write to terminal.")?;
        }
        Ok(())
    }
}

impl State {
    fn draw(&mut self, terminal: &mut Terminal<impl Backend>) -> io::Result<()> {
        terminal.draw(|f| f.render_widget(self, f.size()))?;
        Ok(())
    }
}

fn handle_message(
    message: Message,
    State { entries, ui }: &mut State,
    pending_favorite_change: &mut Option<u64>,
) -> Result<(), CommandError> {
    let UiEntries {
        loaded_entries,
        search_results,
        loaded_state,
        search_state,
    } = entries;
    let UiState {
        details_requested,
        detailed_entry,
        detail_scroll: _,
        query: _,
        show_help: _,
    } = ui;
    match message {
        Message::FatalDbOpen(e) => return Err(e)?,
        Message::FatalServerConnect(e) => return Err(e)?,
        Message::Error(e) => return Err(e),
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
            }
            if let Some(id) = pending_favorite_change.take() {
                if let Some(index) = active_entries!(entries, ui)
                    .iter()
                    .position(|e| e.entry.id() == id)
                {
                    active_list_state!(entries, ui).select(Some(index));
                }
            }
        }
        Message::EntryDetails { id, result } => {
            if *details_requested == Some(id) {
                *detailed_entry = Some(result);
            }
        }
        Message::SearchResults(entries) => {
            *search_results = entries;
            search_state.select_first();
        }
        Message::FavoriteChange(id) => *pending_favorite_change = Some(id),
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn handle_event(event: &Event, state: &mut State, requests: &Sender<Command>) -> bool {
    let State { entries, ui } = state;
    let refresh = || {
        let _ = requests
            .send(Command::RefreshDb)
            .and_then(|()| requests.send(Command::LoadFirstPage));
    };
    macro_rules! unselect {
        () => {{
            ui.details_requested = None;
            ui.detailed_entry = None;
        }};
    }
    macro_rules! select {
        () => {{
            if let Some(&UiEntry { entry, ref cache }) = selected_entry!(entries, ui) {
                ui.details_requested = Some(entry.id());
                ui.detailed_entry = None;
                ui.detail_scroll = 0;
                let _ = requests.send(Command::GetDetails {
                    entry,
                    with_text: matches!(cache, UiEntryCache::Text { .. }),
                });
            }
        }};
    }

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
                        if ui.details_requested.is_some() {
                            unselect!();
                        } else {
                            return true;
                        }
                    }
                    Char('q') => return true,
                    Char('c') if modifiers == KeyModifiers::CONTROL => return true,
                    Char('h') | Left => unselect!(),
                    Char('j') | Down => {
                        let state = active_list_state!(entries, ui);
                        let next = state.selected().map_or(0, |i| {
                            if i + 1 == active_entries!(entries, ui).len() {
                                0
                            } else {
                                i + 1
                            }
                        });
                        state.select(Some(next));
                    }
                    Char('J') => {
                        ui.detail_scroll = ui.detail_scroll.saturating_add(1);
                    }
                    Char('k') | Up => {
                        let state = active_list_state!(entries, ui);
                        let previous = state.selected().map_or(usize::MAX, |i| {
                            if i == 0 {
                                active_entries!(entries, ui).len() - 1
                            } else {
                                i - 1
                            }
                        });
                        state.select(Some(previous));
                    }
                    Char('K') => {
                        ui.detail_scroll = ui.detail_scroll.saturating_sub(1);
                    }
                    Char('l') | Right => select!(),
                    Char(' ') => {
                        if ui.details_requested.is_some() {
                            unselect!();
                        } else {
                            select!();
                        }
                    }
                    Char('/' | 's') => {
                        // TODO Search
                    }
                    Char('f') => {
                        if let Some(&UiEntry { entry, cache: _ }) = selected_entry!(entries, ui) {
                            match entry.ring() {
                                RingKind::Favorites => {
                                    let _ = requests.send(Command::Unfavorite(entry.id()));
                                }
                                RingKind::Main => {
                                    let _ = requests.send(Command::Favorite(entry.id()));
                                }
                            }
                            refresh();
                        }
                    }
                    Char('d') => {
                        if let Some(&UiEntry { entry, cache: _ }) = selected_entry!(entries, ui) {
                            let _ = requests.send(Command::Delete(entry.id()));
                            refresh();
                        }
                    }
                    Char('?') => {
                        ui.show_help ^= true;
                    }
                    Char('x') => {
                        // TODO search with regex
                    }
                    Char('r') => {
                        refresh();
                        if modifiers == KeyModifiers::CONTROL {
                            *state = State::default();
                        } else {
                            ui.detail_scroll = 0;
                        }
                        return false;
                    }
                    Enter => {
                        // TODO paste
                    }
                    _ => {}
                }
            }
        }
        Event::FocusGained => {
            refresh();
        }
        _ => {}
    }
    if let Some(detail_id) = ui.details_requested
        && let Some(&UiEntry { entry, cache: _ }) = selected_entry!(entries, ui)
        && detail_id != entry.id()
    {
        select!();
    }
    false
}

impl Widget for &mut State {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [header_area, rest_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            if self.ui.show_help {
                Constraint::Length(3)
            } else {
                Constraint::Length(0)
            },
        ])
        .areas(area);

        let [entry_list_area, _padding, selected_entry_area] =
            if self.ui.details_requested.is_none() {
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
            .areas(rest_area);

        State::render_title(header_area, buf);
        self.render_entries(entry_list_area, buf);
        self.render_selected_entry(selected_entry_area, buf);
        self.render_footer(footer_area, buf);
    }
}

fn ui_entry_line(UiEntry { entry: _, cache }: &UiEntry) -> Line {
    match cache {
        UiEntryCache::Text { one_liner } => Line::raw(&**one_liner),
        UiEntryCache::Image { .. } => Line::raw("Image: not yet supported"), // TODO
        UiEntryCache::Binary { mime_type, context } => Line::raw(format!(
            "Unable to display format of type {mime_type:?} from {context:?}."
        )),
        UiEntryCache::Error(e) => Line::raw(format!("Error: {e}\nDetails: {e:#?}")),
    }
}

impl State {
    fn render_entries(&mut self, area: Rect, buf: &mut Buffer) {
        let Self { entries, ui } = self;

        let outer_block = Block::new()
            .title_alignment(Alignment::Center)
            .borders(Borders::TOP)
            .title("Entries");
        let inner_block = Block::new().borders(Borders::NONE);
        let inner_area = outer_block.inner(area);

        outer_block.render(area, buf);

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

    fn render_selected_entry(&mut self, area: Rect, buf: &mut Buffer) {
        let Self { entries, ui } = self;
        let Some(UiEntry { entry, cache: _ }) = active_list_state!(entries, ui)
            .selected()
            .map(|i| &active_entries!(entries, ui)[i])
        else {
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
                .title(format!(
                    "{} ({}{})",
                    match entry.ring() {
                        RingKind::Favorites => "Favorite entry",
                        RingKind::Main => "Entry",
                    },
                    entry.id(),
                    if mime_type.is_empty() {
                        String::new()
                    } else {
                        format!("; {mime_type}")
                    }
                ))
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
        Paragraph::new(ui.detailed_entry.as_ref().map_or("Loading…", |r| match r {
            Ok(DetailedEntry {
                mime_type: _,
                full_text,
            }) => full_text.as_deref().unwrap_or("Binary data."),
            Err(_) => &error,
        }))
        .block(inner_block)
        .wrap(Wrap { trim: false })
        .scroll((ui.detail_scroll, 0))
        .render(inner_area, buf);
    }

    fn render_title(area: Rect, buf: &mut Buffer) {
        Paragraph::new("Ringboard")
            .bold()
            .centered()
            .render(area, buf);
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        if !self.ui.show_help {
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
            "Use ↓↑ to move, ←→ to (un)select, / to search, x to search with RegEx, r to reload, \
             f to (un)favorite, d to delete.",
        )
        .wrap(Wrap { trim: true })
        .block(inner_block)
        .centered()
        .render(inner_area, buf);
    }
}
