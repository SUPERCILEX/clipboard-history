use ringboard_sdk::{
    ClientError,
    search::CancellationTokenSink,
    ui_actor::{CommandError, DetailedEntry, SearchKind, UiEntry},
};

use crate::theme::ThemeManager;

/// The complete application model (TEA Model).
pub struct State {
    pub entries: UiEntries,
    pub ui: UiState,
    pub theme: ThemeManager,
}

impl State {
    pub fn new() -> Self {
        Self {
            entries: UiEntries::default(),
            ui: UiState::default(),
            theme: ThemeManager::new(),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct UiEntries {
    pub loaded_entries: Box<[UiEntry]>,
    pub search_results: Box<[UiEntry]>,
}

#[derive(Default)]
pub struct UiState {
    pub fatal_error: Option<ClientError>,
    pub last_error: Option<CommandError>,
    pub highlighted_id: Option<u64>,
    pub details_requested: Option<u64>,
    pub detailed_entry: Option<DetailedEntry>,
    pub query: String,
    pub search_highlighted_id: Option<u64>,
    pub search_kind: SearchKind,
    pub pending_search_token: Option<CancellationTokenSink>,
    pub active_tab: ActiveTab,
    pub pinned_expanded: bool,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    #[default]
    All,
    Text,
    Images,
    Favorites,
}

impl ActiveTab {
    pub const ALL: [ActiveTab; 4] = [
        ActiveTab::All,
        ActiveTab::Text,
        ActiveTab::Images,
        ActiveTab::Favorites,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ActiveTab::All => "All",
            ActiveTab::Text => "Text",
            ActiveTab::Images => "Images",
            ActiveTab::Favorites => "Pins",
        }
    }
}
