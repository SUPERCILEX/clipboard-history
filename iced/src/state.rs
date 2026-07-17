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
    pub settings: SettingsState,
}

impl State {
    pub fn new() -> Self {
        Self {
            entries: UiEntries::default(),
            ui: UiState::default(),
            theme: ThemeManager::new(),
            settings: SettingsState::default(),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    /// Clears entries and transient UI state (e.g. when hiding to the
    /// background), keeping the resolved theme and loaded settings.
    pub fn reset(&mut self) {
        self.entries = UiEntries::default();
        self.ui = UiState::default();
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
    pub hovered_id: Option<u64>,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    #[default]
    All,
    Text,
    Images,
    Favorites,
    Settings,
}

impl ActiveTab {
    pub const ALL: [ActiveTab; 5] = [
        ActiveTab::All,
        ActiveTab::Text,
        ActiveTab::Images,
        ActiveTab::Favorites,
        ActiveTab::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ActiveTab::All => "All",
            ActiveTab::Text => "Text",
            ActiveTab::Images => "Images",
            ActiveTab::Favorites => "Favorites",
            ActiveTab::Settings => "Settings",
        }
    }
}

/// Buffers and status for the Settings tab, which surfaces the same
/// on-disk server configuration and maintenance actions the `ringboard`
/// CLI exposes (`ringboard configure server`, `ringboard gc`).
#[derive(Default)]
pub struct SettingsState {
    pub loaded: bool,
    pub max_main_entries: String,
    pub max_favorite_entries: String,
    pub gc_max_wasted_bytes: String,
    pub status: Option<Result<String, String>>,
    pub saving: bool,
    pub running_gc: bool,
}
