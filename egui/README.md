# Ringboard egui

This binary is a Ringboard client that provides a GUI built with
[egui](https://github.com/emilk/egui).

## Usage instructions

- Press <kbd>Enter</kbd> to paste.
  - Use <kbd>Ctrl</kbd> + <kbd>N</kbd> to paste the `N`<sup>th</sup> normal entry.
  - Use <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>N</kbd> to paste the `N`<sup>th</sup> favorited
    entry.
  - TODO ⚠️ paste is not implemented yet.
- Right click entries or press <kbd>Space</kbd> to see details.
- Type <kbd>/</kbd> to search.
  - Use <kbd>Alt</kbd> + <kbd>X</kbd> to switch to RegEx search.
  - Note that the search input text font will be monospaced when in RegEx mode.
- Use <kbd>Ctrl</kbd> + <kbd>R</kbd> to manually reload the database.
