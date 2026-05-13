# Ringboard GNOME Shell Extension

A native GNOME Shell Wayland extension that serves dual purposes:

1. **Clipboard intake channel** — captures clipboard content from GNOME Wayland via `St.Clipboard` and submits new entries to the Ringboard server through FFI bindings to the Rust `client-sdk`.
2. **Minimal UI** — panel indicator with a dropdown popup for browsing clipboard history, searching, and pasting.

## Architecture

```
GNOME Shell (Wayland)
  └── extension.js            JS extension entry point
        ├── St.Clipboard      Wayland clipboard monitoring
        ├── PanelMenu.Button  Panel indicator + popup UI
        └── GModule FFI  ──►  libringboard_gnome_ffi.so
                                └── client-sdk (Rust)
                                      └── Ringboard server (Unix socket)
```

## Files

| File | Purpose |
|---|---|
| `extension/extension.js` | Main extension: enable/disable lifecycle, clipboard intake, panel UI |
| `extension/dataStructures.js` | In-memory linked list for clipboard history entries |
| `extension/confirmDialog.js` | Modal confirmation dialog helper |
| `extension/settingsFields.js` | GSettings key name constants |
| `extension/stylesheet.css` | Extension-specific CSS |
| `extension/metadata.json` | GNOME Shell extension manifest |
| `ffi/src/lib.rs` | Rust FFI layer: C-ABI exports for init/destroy/add/remove |
| `ffi/Cargo.toml` | Rust crate definition (cdylib) |

## Building the FFI library

```sh
cd gnome-shell/ffi
cargo build --release
cp target/release/libringboard_gnome_ffi.so ../extension/
```

## Installing

```sh
# Copy the extension directory to GNOME Shell extensions path
cp -r extension ~/.local/share/gnome-shell/extensions/ringboard@clipboard-history/
gnome-extensions enable ringboard@clipboard-history
```

## Requirements

- GNOME Shell 46+
- Ringboard server running as a systemd user service
- Wayland session (X11 clipboard handled by the separate `x11` crate)

## Design decisions

- **No settings UI** (phase 1): all configuration uses sensible defaults.
- **No store.js**: history is kept in-memory only; the Ringboard server is the durable store.
- **Browse-only fallback**: if the Ringboard server is unavailable on startup, the extension disables the intake channel but still shows previously captured entries from the in-memory cache.
