use ::image as image_crate;
use iced::keyboard;

use crate::state::ActiveTab;

/// The only way to change application state (TEA Msg).
#[derive(Debug, Clone)]
pub enum Message {
    /// Poll the controller response channel.
    Tick,
    /// Global keyboard event.
    KeyEvent(keyboard::Event),
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
}
