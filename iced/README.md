# Ringboard iced

<a href="https://crates.io/crates/clipboard-history-iced">![Crates.io Version](https://img.shields.io/crates/v/clipboard-history-iced)</a>

This binary is a Ringboard client that provides a GUI built with
[iced](https://github.com/iced-rs/iced), using [native-theme-iced](https://docs.rs/native-theme-iced)
to match your system theme.

## Usage instructions

- Type to search; the search box is focused automatically whenever the window is focused, so you
  can start typing right away.
  - Use <kbd>Alt</kbd> + <kbd>X</kbd> or <kbd>Alt</kbd> + <kbd>M</kbd> to cycle the search kind
    (plain text → RegEx → MIME type).
- Press <kbd>Enter</kbd> to paste the highlighted entry.
  - Use <kbd>Ctrl</kbd> + <kbd>N</kbd> to paste the `N`<sup>th</sup> entry.
- Use <kbd>Up</kbd>/<kbd>Down</kbd> to move the highlight, and <kbd>Left</kbd>/<kbd>Right</kbd> to
  collapse/expand the Favorites section.
- Hover an entry (or highlight it with the keyboard) to reveal its delete and show-details
  buttons; the favorite star is always visible.
- Use <kbd>Ctrl</kbd> + <kbd>D</kbd> to toggle details for the highlighted entry.
- Use <kbd>Alt</kbd> + <kbd>1</kbd>-<kbd>5</kbd> to jump directly to a tab
  (All/Text/Images/Favorites/Settings), or <kbd>Ctrl</kbd> + <kbd>Tab</kbd> /
  <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>Tab</kbd> to cycle through them.
- Use <kbd>Ctrl</kbd> + <kbd>R</kbd> to manually reload the database.
- Press <kbd>Escape</kbd> to clear the current search, or to close the app if the search is
  already empty.

## Settings tab

The Settings tab surfaces the same knobs the `ringboard` CLI already exposes, without dropping to
a terminal:

- **Server limits** (`ringboard configure server`): the max main-ring and favorites-ring entry
  counts, read from and written to the server's `config.toml`. As with the CLI, the server needs
  restarting for a change to take effect.
- **Maintenance** (`ringboard gc`): trigger a garbage-collection pass with a given "max wasted
  bytes" threshold (0 forces a full compaction and duplicate cleanup).

Wiping the database and viewing detailed fragmentation stats are still CLI-only for now
(`ringboard wipe` / `ringboard debug stats`) — wiping in particular tears down the server and the
directory this client has open, which isn't something to do safely from a running GUI session
without more surgery than a first pass warrants.

## Performance

Unlike a fixed-interval redraw loop, this client only wakes up in response to real events:
keyboard/window events and messages pushed from the background controller thread each deliver
their own wakeup, so the app does no work at all while sitting idle (no background polling
timer). This keeps idle CPU usage effectively at zero.

Note that, unlike the [egui client](../egui), this client currently exits fully when closed rather
than resuming from a resident background process, so it doesn't yet have an instant-relaunch
`toggle` command.
