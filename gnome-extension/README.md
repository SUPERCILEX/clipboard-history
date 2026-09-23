# Ringboard GNOME Shell extension

Companion extension for the [Ringboard](../README.md) X11 watcher. It exists for
one reason: **GNOME Wayland cannot be pasted into without it.**

## Why it is needed

On GNOME, the X11 watcher monitors the clipboard through XWayland, and injects
the paste keystroke with XTEST. GNOME routes XWayland XTEST through
`org.freedesktop.portal.RemoteDesktop`, so every paste pops up an
"Allow remote interaction" dialog — and XTEST cannot reach native Wayland
applications at all.

XTEST on X11 sessions and `zwp_virtual_keyboard_v1` on wlroots-based compositors
(Sway, Hyprland, COSMIC, …) are unaffected and remain the correct mechanisms
there. This extension only covers GNOME Wayland, where neither is available:
Mutter does not implement `zwp_virtual_keyboard_manager_v1` or
`ext_data_control_manager_v1`.

## How it works

The extension runs inside gnome-shell and creates a Clutter virtual input device,
which needs no portal and no root:

```js
const device = Clutter.get_default_backend()
  .get_default_seat()
  .create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
device.notify_keyval(time, Clutter.KEY_Shift_L, Clutter.KeyState.PRESSED);
```

It exports a single D-Bus method:

```
bus name:    dev.alexsaveau.ringboard.Paste
object path: /dev/alexsaveau/ringboard/Paste
interface:   dev.alexsaveau.ringboard.Paste
method:      Paste()
```

The watcher calls `Paste()`, and the extension sends `Shift+Insert`
(`Ctrl+Shift+Insert` when the focused surface is a terminal), after waiting for
Ringboard's own window to lose focus so the keystroke lands in the application
you came from.

It deliberately does not read, store, or manage the clipboard — it only injects
a keystroke.

## Installing

```sh
gnome-extensions install --force gnome-extension.zip   # or point at this dir
gnome-extensions enable ringboard-paste@alexsaveau.dev
```

`install-with-cargo-systemd.sh` does this automatically when it detects GNOME on
Wayland. On GNOME, a freshly installed extension is not loaded until gnome-shell
picks it up: log out and back in (or restart the shell on X11) before the first
paste.

Check that it is running:

```sh
gnome-extensions info ringboard-paste@alexsaveau.dev
gdbus call --session --dest dev.alexsaveau.ringboard.Paste \
  --object-path /dev/alexsaveau/ringboard/Paste \
  --method dev.alexsaveau.ringboard.Paste.Paste
```

## Compatibility

Requires GNOME Shell 45 or newer (ESM extensions). Tested on GNOME Shell 50.1
(Ubuntu 26.04). GNOME's extension APIs change between releases, so the supported
list in `metadata.json` should be widened only after testing.

## License and attribution

`Apache-2.0 OR GPL-3.0-or-later`. GNOME Shell is GPL-2.0-or-later, so extensions
must be distributed under compatible terms; Ringboard itself is Apache-2.0, which
is compatible with GPL-3.0.

The injection technique is adapted from two MIT-licensed extensions, with thanks:

- [`SUPERCILEX/gnome-clipboard-history`](https://github.com/SUPERCILEX/gnome-clipboard-history)
  — the same author's earlier clipboard manager, and the source of the
  `notify_key`/`notify_keyval` "paste hack".
- [`Tudmotu/gnome-shell-extension-clipboard-indicator`](https://github.com/Tudmotu/gnome-shell-extension-clipboard-indicator)
  — the source of the terminal detection (`content_purpose === TERMINAL`).
