# Ringboard egui

<a href="https://crates.io/crates/clipboard-history-egui">![Crates.io Version](https://img.shields.io/crates/v/clipboard-history-egui)</a>

This binary is a Ringboard client that provides a GUI built with
[egui](https://github.com/emilk/egui).

## Suggested workflow

To reduce startup latency, closing the application sends it to the background rather than killing
it. Thus, it is suggested to bind a shortcut that executes the following command for fast clipboard
launches:

```shell
# Run this command to generate the command that goes in the shortcut
bash -c 'echo /bin/sh -c \"ps -p \`cat /tmp/.ringboard/$USERNAME.egui-sleep 2\> /dev/null\` \> /dev/null 2\>\&1 \&\& exec rm -f /tmp/.ringboard/$USERNAME.egui-sleep \|\| exec $(which ringboard-egui)\"'
```

## Usage instructions

- Press <kbd>Enter</kbd> to paste.
  - Use <kbd>Ctrl</kbd> + <kbd>N</kbd> to paste the `N`<sup>th</sup> entry.
- Right click entries or press <kbd>Space</kbd> to see details.
- Type <kbd>/</kbd> to search.
  - Use <kbd>Alt</kbd> + <kbd>X</kbd> to switch to RegEx search.
  - Note that the search input text font will be monospaced when in RegEx mode.
- Use <kbd>Ctrl</kbd> + <kbd>R</kbd> to manually reload the database.
