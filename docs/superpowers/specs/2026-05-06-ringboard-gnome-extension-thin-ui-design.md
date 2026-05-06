# Ringboard GNOME Shell Extension — Thin UI Redesign

## Goal

Restructure the `ringboard@clipboard-history` GNOME Shell extension as a thin
UI over the `ringboard` CLI. The server is the canonical store; the extension
holds no long-lived entry state. Fix two user-visible defects in the process:
search misses old entries, and the extension captures `PRIMARY` selection
(text-highlight) when it should not by default.

## Background

The current extension eagerly loads up to 12,000+ entries from
`ringboard search --json ""` into an in-memory `LinkedList` at `enable()`,
then prunes it with `MAX_REGISTRY_LENGTH = 500` on every clipboard change.
The prune loop accesses already-disposed `PopupMenuItem` objects, producing
runtime errors and an inconsistent visible list. Search iterates the
in-memory list, so it sees only the post-prune contents.

The existing `ringboard` UIs (`tui` and `egui`) take a different approach:
they bypass the CLI and use the `client-sdk` directly with a memory-mapped
reader. They expose two operations — `LoadFirstPage` (favorites + last 100
main entries, fixed) and `Search` (returns all matches) — and rely on a
viewport scroller to render long lists. There is no `LoadNextPage` command;
to find an entry older than 100, the user searches.

Our extension cannot easily call the Rust SDK from GJS, so it must shell out
to the `ringboard` CLI. We retain pagination in the extension UX (prev/next
buttons in the dropdown) because a panel popover is not a good place for a
viewport with thousands of items.

## In scope

- `gnome-shell/extension/extension.js` — full restructure
- `gnome-shell/extension/dataStructures.js` — strip down to helpers actually used
- `gnome-shell/extension/schemas/org.gnome.shell.extensions.ringboard.gschema.xml` — new
- `gnome-shell/extension/prefs.js` — new
- `gnome-shell/extension/metadata.json` — `settings-schema` field
- `gnome-shell/extension/stylesheet.css` — minor adjustments if menu structure changes

## Out of scope

- Rust FFI crate under `gnome-shell/ffi/` — left as-is, currently unused by the
  extension; do not touch in this work
- X11 clipboard intake — handled separately by `ringboard-x11.service`
- Image clipboard entries — text only for this iteration
- Cosmetic settings (`topbar-preview-size`, `topbar-display-mode`,
  `disable-down-arrow`, `window-width-percentage`, `notify-on-copy`)
- Password MIME filtering — server / OS handles
- `history-size` setting — server controls retention

## Architecture

The extension consists of three orthogonal components plus the panel button
that wires them together:

```
┌─────────────────────────────────────────────────┐
│           GNOME Shell extension                 │
│                                                 │
│  ┌──────────────┐    ┌────────────────────┐     │
│  │ Clipboard    │    │ Panel menu UI      │     │
│  │ intake       │    │ (transient state   │     │
│  │ (Wayland)    │    │  per menu open)    │     │
│  └──────┬───────┘    └────────┬───────────┘     │
│         │ on Ctrl-C            │ on open / type  │
│         ▼                      ▼                 │
│  ┌─────────────────────────────────────────┐    │
│  │       Subprocess client (Gio)           │    │
│  │  add | search | move-to-front | remove  │    │
│  │  | wipe | last (probe)                  │    │
│  └────────┬────────────────────────────────┘    │
└───────────┼─────────────────────────────────────┘
            │ async stdio
            ▼
       ┌──────────────┐
       │ /usr/bin     │
       │ /ringboard   │  ⇄  ringboard-server (canonical state)
       └──────────────┘
```

### Component 1: SubprocessClient

Wraps the `ringboard` CLI. All methods async, returning Promises. No
`GLib.idle_add` indirection — `Gio.Subprocess.communicate_utf8_async` already
runs off the main thread.

```text
class SubprocessClient {
  constructor(binaryPath)
  probe()                          → Promise<bool>      // ringboard last
  add(text)                        → Promise<id|null>   // ringboard add -
  search(query)                    → Promise<Entry[]>   // ringboard search --json
  moveToFront(id)                  → Promise<bool>      // ringboard move-to-front
  remove(id)                       → Promise<bool>      // ringboard remove
  wipe()                           → Promise<bool>      // ringboard wipe
}
```

`Entry` is `{ id: number, kind: string, data: string }` matching the JSON
shape returned by `ringboard search --json`. Non-text entries
(`kind === "Image"` or similar) are passed through verbatim; rendering filters
them at the menu layer.

CLI binary lookup: hardcoded candidates `/usr/bin/ringboard`,
`/usr/local/bin/ringboard`, `~/.cargo/bin/ringboard`. First executable wins.
No PATH search.

### Component 2: ClipboardIntake

Listens to clipboard owner-changed signals on the GNOME display selection.
Always listens to `SELECTION_CLIPBOARD`. Listens to `SELECTION_PRIMARY` only
when the `process-primary-selection` setting is true; reconfigures
dynamically when the setting changes.

```text
class ClipboardIntake {
  constructor(client, settings)
  enable()
  disable()
  // private:
  //   _onSelectionChanged(type)
  //   _onSettingsChanged()      ← reconnects PRIMARY signal as needed
  //   _debouncing               ← suppresses own writes from looping back
}
```

Intake flow:
1. Owner-changed fires for `SELECTION_CLIPBOARD` (or `SELECTION_PRIMARY` if
   that listener is connected).
2. If `_debouncing > 0`, decrement and return.
3. If `private-mode` setting is true, return.
4. Read text via `St.Clipboard.get_text(type)`.
5. If text is empty or whitespace-only, return.
6. If `strip-text` setting is true, `text = text.trim()`.
7. Call `client.add(text)`. Errors are logged via `console.warn`; not
   surfaced to the user.

The intake does *not* update any UI state. The next menu open re-fetches from
the server.

### Component 3: MenuController

Owns the dropdown's transient state. State exists only between
`onMenuOpen()` and `onMenuClose()`.

```text
class MenuController {
  constructor(client, settings, historySection)

  // transient state (alive only while menu is open)
  currentQuery     // ""
  currentOffset    // 0
  resultIds        // null when menu closed; full Entry[] while open
  selectedIndex    // null or index into the current page
  fetchGeneration  // increments per fetch; results from older fetches discarded

  // page size
  static PAGE_SIZE = 50

  // menu lifecycle
  onMenuOpen()
  onMenuClose()

  // user actions
  setQuery(q)
  nextPage()
  prevPage()
  selectAndPaste(entry)
  removeEntry(entry)
  clearAll()
}
```

`onMenuOpen()`:
1. Reset `currentQuery = ""`, `currentOffset = 0`, `selectedIndex = null`.
2. Increment `fetchGeneration`; capture local `gen = fetchGeneration`.
3. `result = await client.search("")`.
4. If `gen !== fetchGeneration` (stale), discard.
5. `resultIds = result`. `renderPage()`.

`renderPage()`:
1. Clear all children of `historySection`.
2. Slice `pageEntries = resultIds.slice(currentOffset,
   currentOffset + PAGE_SIZE)`.
3. For each entry, build a `PopupMenuItem` with the truncated label
   (`MAX_VISIBLE_CHARS = 200`). Image entries render a placeholder string
   (e.g. `"[image]"`) and are non-activatable.
4. Update prev/next button visibility based on `currentOffset` and
   `resultIds.length`.

`setQuery(q)` debounces 150ms then re-fetches with the new query. Keystrokes
during the debounce window cancel any pending timer.

`nextPage()` / `prevPage()` adjust `currentOffset` by `±PAGE_SIZE`, clamped
to `[0, resultIds.length)`, and call `renderPage()`. No CLI call.

`selectAndPaste(entry)`:
1. Set `St.ClipboardType.CLIPBOARD` to `entry.data`. Set `_debouncing = 1` on
   `ClipboardIntake` (one signal expected; we only set CLIPBOARD now,
   not PRIMARY).
2. If the `move-item-first` setting is true, fire-and-forget
   `client.moveToFront(entry.id)`.
3. If the `paste-on-selection` setting is true, simulate Ctrl-V to the
   focused window via `Clutter.VirtualInputDevice`.
4. Close the menu (default GNOME `PopupMenu` behavior on activation).

`removeEntry(entry)`:
1. `await client.remove(entry.id)`.
2. On success, drop from `resultIds`; call `renderPage()` (offset unchanged;
   the page may shrink by one item).
3. On failure, `console.warn`; entry remains visible.

`clearAll()`:
1. If the `confirm-clear` setting is true, show `ConfirmDialog`; bail on
   cancel.
2. `await client.wipe()`. On success, `resultIds = []`, close the menu. On
   failure, log and keep the menu state.

### Component 4: ClipboardIndicator (panel button)

Glue. Constructs the panel button, the dropdown menu (search entry, scroll
view holding `historySection`, separator, prev/next/clear actions),
instantiates the three components above, wires the search-text-changed
signal to `MenuController.setQuery`, the prev/next/clear buttons to the
matching controller methods, the menu open/close signals to the controller
lifecycle hooks, and the toggle-menu keyboard shortcut to `menu.toggle()`.

When the server probe fails at `enable()`:
- The icon switches to a "disconnected" variant (`network-offline-symbolic`
  or similar).
- The menu, when opened, shows a single non-activatable
  `PopupMenuItem` with text "Ringboard server unavailable".
- `ClipboardIntake.enable()` is not called.
- No retry. The user must reload the extension after fixing the server.

The toggle-menu keybinding is bound only when the `enable-keybindings`
setting is true. Toggling the setting at runtime rebinds.

### Component 5: dataStructures.js

Reduce to only what is referenced by the new code:
- `MAX_VISIBLE_CHARS = 200` constant
- `truncateLabel(text, maxLen)` helper

Drop: `LinkedList`, `LLNode`, all `TYPE_*` constants, `findTextItem`,
`typeFromMime`, `placeholderLabel`. None survive in the thin-UI design.

### Component 6: GSettings schema

New file: `gnome-shell/extension/schemas/org.gnome.shell.extensions.ringboard.gschema.xml`

Keys:

| key | type | default | description |
|-----|------|---------|-------------|
| `paste-on-selection` | b | true | Click → also virtual Ctrl-V; false → only set clipboard |
| `move-item-first` | b | true | Click → also `ringboard move-to-front` |
| `confirm-clear` | b | true | Confirm dialog before `ringboard wipe` |
| `private-mode` | b | false | Pause clipboard intake |
| `enable-keybindings` | b | true | Toggle the panel-menu keybinding |
| `process-primary-selection` | b | **false** | Also intake PRIMARY selection |
| `strip-text` | b | false | `.trim()` before `ringboard add` |
| `enable-typeahead-search` | b | true | Letters typed in dropdown jump into search box |
| `toggle-menu` | as | `["<Super><Shift>v"]` | Toggle-menu accelerator (existing) |

Schema must be compiled to `gschemas.compiled` at install time:
`glib-compile-schemas gnome-shell/extension/schemas/`. The pack step needs
`--schema=org.gnome.shell.extensions.ringboard.gschema.xml`.

### Component 7: prefs.js

GNOME 46+ `ExtensionPreferences` subclass exposing the eight boolean
settings (skip `toggle-menu`; that one is set via the keybinding GUI in
GNOME Settings) as a single Adw.PreferencesPage with one Adw.PreferencesGroup
containing one Adw.SwitchRow per setting. No fancy layout.

### metadata.json

Add the field `"settings-schema": "org.gnome.shell.extensions.ringboard"`.

## Data flow summary

| User action | Flow |
|-------------|------|
| Server probe at `enable()` | `client.probe()` once. Failure → disconnected mode |
| Ctrl-C (Wayland) | owner-changed → debounce check → settings check → `client.add()` |
| Open menu | `client.search("")` → render page 0 |
| Type in search box | debounce 150ms → `client.search(query)` → render page 0 |
| Click prev/next | re-slice cached `resultIds`; no CLI call |
| Click entry | set CLIPBOARD; conditional move-to-front; conditional virtual paste |
| Delete entry (keyboard shortcut on highlighted item) | `client.remove(id)` → drop from `resultIds` → re-render |
| Clear all (button) | confirm dialog (if enabled) → `client.wipe()` |

## Error handling

| Failure | Behavior |
|---------|----------|
| Server probe fails at enable | Disconnected icon, single disabled menu item, intake disabled, no retry |
| `client.search` fails after enable | Render error item in dropdown ("Server unreachable"); keep last good `resultIds` if any; `console.warn` |
| `client.add` fails | `console.warn`; do not retry; no UI surface |
| `client.moveToFront` / `client.remove` / `client.wipe` fails | `console.warn`; entry stays visible in the case of `remove` |
| `ringboard add` exits non-zero with text on stderr | Log stderr; do not insert |
| Subprocess spawn raises (`Gio.Subprocess.new` throws) | Log; treat as non-fatal failure of the operation |

## Testing strategy

This is a GNOME Shell extension. Unit tests in JavaScript are impractical;
the test environment is the live Shell.

Manual verification matrix (the implementation plan will reference these):

1. Server running, fresh extension load: panel icon appears in normal
   state; menu open shows up to 50 most-recent entries.
2. Server running, type a query: dropdown shows up to 50 matches, page
   buttons respect total match count.
3. Server stopped, fresh extension load: panel icon shows disconnected
   state; menu shows "Ringboard server unavailable".
4. Server stopped after extension load, then menu opened: error item in
   dropdown; no crash.
5. Ctrl-C in a Wayland-native app: server has the new entry within 1s.
6. Highlight text in xterm/firefox with `process-primary-selection = false`:
   no new entry on server.
7. Toggle `process-primary-selection = true`, highlight text: entry on server.
8. Toggle `private-mode = true`, Ctrl-C: no entry on server.
9. Toggle `paste-on-selection = false`, click entry: clipboard set, no
   Ctrl-V fired (target window unchanged).
10. Toggle `move-item-first = false`, click entry: server entry order
    unchanged.
11. Click "Clear all" with `confirm-clear = true`: dialog appears; cancel
    leaves entries; confirm wipes.
12. With server holding 12k+ entries: menu open completes within ~1s; no
    runtime errors in `journalctl`; prev/next page traverse correctly.
13. Delete a single entry: it disappears from the dropdown and from the
    server.

## Removal checklist

The following must be deleted from `extension.js` (no dead-code retention):

- `_loadHistoryFromServer` and the eager-load call from `_init`
- `MAX_REGISTRY_LENGTH` and the prune loop in `_processClipboardContent`
- `findTextItem`-based dedup
- `nextId` counter, generation counter for add/delete races
- `_queryPrimaryClipboard` (replaced by setting-driven branch in
  `ClipboardIntake`)
- All `LinkedList` / `LLNode` usage
- The CLI-binary PATH-search fallback and `~/.cargo/bin` candidate path
  (keep only system paths)
- `_findCliBinary`'s use of `GLib.spawn_command_line_sync`
- The "find native FFI" path (`_tryLoadNativeFfi`) — the FFI crate is
  out of scope for this design
- `_processClipboardContent`'s `persistToServer` parameter
- The MIME-based "secret" detection in `_shouldAbortClipboardQuery`
- `MAX_ENTRY_STORE_CHARS` truncation

## Risks

- **GSettings schema requires recompilation on install.** The
  `gnome-extensions pack` step needs `--schema=…` and the schema XML must be
  in `schemas/`. Mistakes here yield silent setting-default behavior with no
  user-visible error.
- **`ringboard search --json ""` returns the whole DB.** With 12k+ entries
  that is ~21MB of JSON parsed once per menu-open. Acceptable for an
  interactive open but may stutter on slower machines. If it does, follow-up
  work: ask upstream for an offset/limit on `ringboard search`.
- **Server-stop after extension start.** We do not poll for re-connection;
  if the user stops/starts the server the extension stays in degraded mode
  until reload. Reasonable for a thin UI; flagged here for awareness.
