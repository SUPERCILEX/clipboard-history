use std::sync::Arc;

use ::image as image_crate;
use iced::{keyboard, window};
use ringboard_sdk::ui_actor::Message as ControllerMessage;

use crate::state::ActiveTab;

/// The only way to change application state (TEA Msg).
#[derive(Debug, Clone)]
pub enum Message {
    /// A message pushed from the background controller thread.
    ///
    /// Wrapped in an `Arc` since widgets (e.g. buttons) require `Message:
    /// Clone` and `ringboard_sdk`'s controller message type isn't `Clone`.
    Controller(Arc<ControllerMessage>),
    /// Global keyboard event.
    KeyEvent(keyboard::Event),
    /// Window-level event (focus changes, etc).
    WindowEvent(window::Id, window::Event),
    /// The id of this app's window, resolved once at boot.
    WindowIdResolved(Option<window::Id>),
    /// Another instance of the app asked us to wake up and show ourselves.
    WakeRequested,
    /// Async image decode completed.
    ImageDecoded(u64, Result<image_crate::DynamicImage, String>),
    /// Search query changed.
    SearchChanged(String),
    /// Cycle the search kind.
    SearchKindToggled,
    /// Select a tab directly.
    TabSelected(ActiveTab),
    /// Move to the next tab.
    TabNext,
    /// Move to the previous tab.
    TabPrev,
    /// Toggle the pinned section expansion.
    PinnedToggled,
    /// Paste the clicked entry.
    EntryClicked(u64),
    /// Toggle favorite status of an entry.
    FavoriteToggled(u64),
    /// Delete an entry.
    DeleteEntry(u64),
    /// Open the detail panel for an entry.
    DetailRequested(u64),
    /// Close the detail panel.
    DetailClosed,
    /// Refresh entries and clear caches.
    Refresh,
    /// Paste the entry at the given navigation index.
    FastPaste(u64),
    /// Dismiss the transient error banner.
    DismissError,
    /// Pointer entered or left an entry row.
    EntryHovered(Option<u64>),
    /// The on-disk server config finished loading (or failed to).
    SettingsLoaded(Result<(u32, u32), String>),
    /// The "max main entries" field changed.
    SettingsMaxMainChanged(String),
    /// The "max favorite entries" field changed.
    SettingsMaxFavoritesChanged(String),
    /// The user asked to persist the server config to disk.
    SettingsSaveRequested,
    /// The server config finished saving (or failed to).
    SettingsSaved(Result<(), String>),
    /// The "max wasted bytes" GC threshold field changed.
    SettingsGcBytesChanged(String),
    /// The user asked to run garbage collection now.
    SettingsGcRequested,
}
