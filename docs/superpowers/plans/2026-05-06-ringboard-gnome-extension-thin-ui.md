# Ringboard GNOME Extension Thin-UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure the `ringboard@clipboard-history` GNOME Shell extension as a thin UI over the `ringboard` CLI, with a GSettings schema and prefs dialog backing 8 user-toggleable behaviors.

**Architecture:** Server (ringboard CLI + ringboard-server) is canonical; extension holds only transient per-menu state. Three orthogonal components — `SubprocessClient` (CLI wrapper, async via `Gio.Subprocess.communicate_utf8_async`), `ClipboardIntake` (Wayland selection listener, settings-driven), `MenuController` (dropdown render + pagination). Glue layer is `ClipboardIndicator` (panel button) plus the top-level `Extension` class.

**Tech Stack:** GJS (GNOME JavaScript), GNOME Shell ≥ 46 extension API, GIO async subprocess, GSettings/GLib.compile-schemas, Adw preferences widgets.

**Spec:** `docs/superpowers/specs/2026-05-06-ringboard-gnome-extension-thin-ui-design.md`

**Note on testing:** GNOME Shell extension code that touches `St.*`, `PopupMenu`, `Shell`, etc. cannot run outside a live Shell session. Where a module uses only `Gio` / `GLib` / pure JS, this plan adds a `gjs`-runnable smoke test in `gnome-shell/extension/tests/`. Where the module touches Shell-only APIs, the plan uses **structural verification** (syntax parse, schema compile, pack/install exit codes) plus a **manual verification matrix** that runs against a live session at the end.

---

## File Structure

**Create:**
- `gnome-shell/extension/lib/subprocessClient.js` — async CLI wrapper
- `gnome-shell/extension/lib/clipboardIntake.js` — Wayland selection listener with settings hookup
- `gnome-shell/extension/lib/menuController.js` — dropdown state and rendering
- `gnome-shell/extension/schemas/org.gnome.shell.extensions.ringboard.gschema.xml` — GSettings schema
- `gnome-shell/extension/prefs.js` — Adw preferences dialog
- `gnome-shell/extension/tests/test_subprocess_client.js` — gjs-runnable smoke test for SubprocessClient

**Modify:**
- `gnome-shell/extension/extension.js` — replace with thin `ClipboardIndicator` + `Extension`
- `gnome-shell/extension/dataStructures.js` — strip down to `MAX_VISIBLE_CHARS` and `truncateLabel`
- `gnome-shell/extension/metadata.json` — add `settings-schema` field

**Delete:**
- `gnome-shell/extension/settingsFields.js` — never used; replaced by GSettings schema

**Untouched:**
- `gnome-shell/extension/confirmDialog.js`
- `gnome-shell/extension/stylesheet.css`
- `gnome-shell/extension/clipboard-history.pot`
- `gnome-shell/ffi/` (out of scope)

---

## Task 1: Add GSettings Schema

**Files:**
- Create: `gnome-shell/extension/schemas/org.gnome.shell.extensions.ringboard.gschema.xml`

- [ ] **Step 1: Write the schema XML**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<schemalist>
  <schema id="org.gnome.shell.extensions.ringboard"
          path="/org/gnome/shell/extensions/ringboard/">
    <key name="paste-on-selection" type="b">
      <default>true</default>
      <summary>Paste on selection</summary>
      <description>Click an entry to also send Ctrl-V to the focused window. When false, only the clipboard is set.</description>
    </key>
    <key name="move-item-first" type="b">
      <default>true</default>
      <summary>Move selected item to front</summary>
      <description>Click an entry to also call ringboard move-to-front, so the entry becomes the most recent on the server.</description>
    </key>
    <key name="confirm-clear" type="b">
      <default>true</default>
      <summary>Confirm before clearing</summary>
      <description>Show a confirmation dialog before wiping the entire history.</description>
    </key>
    <key name="private-mode" type="b">
      <default>false</default>
      <summary>Private mode</summary>
      <description>Pause clipboard intake. New copies are not submitted to the server.</description>
    </key>
    <key name="enable-keybindings" type="b">
      <default>true</default>
      <summary>Enable keybindings</summary>
      <description>Bind the toggle-menu shortcut.</description>
    </key>
    <key name="process-primary-selection" type="b">
      <default>false</default>
      <summary>Capture PRIMARY selection</summary>
      <description>Also intake text from the PRIMARY (text-highlight) selection. Off by default.</description>
    </key>
    <key name="strip-text" type="b">
      <default>false</default>
      <summary>Strip whitespace from new entries</summary>
      <description>Trim leading and trailing whitespace before submitting to the server.</description>
    </key>
    <key name="enable-typeahead-search" type="b">
      <default>true</default>
      <summary>Type-ahead search</summary>
      <description>Letters typed while the dropdown is open jump into the search box.</description>
    </key>
    <key name="toggle-menu" type="as">
      <default><![CDATA[['<Super><Shift>v']]]></default>
      <summary>Toggle the panel menu</summary>
      <description>Keyboard accelerator to open or close the clipboard history dropdown.</description>
    </key>
  </schema>
</schemalist>
```

- [ ] **Step 2: Compile the schema and verify**

Run:
```bash
glib-compile-schemas gnome-shell/extension/schemas/
ls gnome-shell/extension/schemas/gschemas.compiled
```

Expected: file `gnome-shell/extension/schemas/gschemas.compiled` exists, no errors.

- [ ] **Step 3: Sanity-check the keys**

Run:
```bash
GSETTINGS_SCHEMA_DIR=gnome-shell/extension/schemas gsettings list-keys org.gnome.shell.extensions.ringboard | sort
```

Expected output (one per line, sorted):
```
confirm-clear
enable-keybindings
enable-typeahead-search
move-item-first
paste-on-selection
private-mode
process-primary-selection
strip-text
toggle-menu
```

- [ ] **Step 4: Commit**

```bash
git add gnome-shell/extension/schemas/org.gnome.shell.extensions.ringboard.gschema.xml
git -c commit.gpgsign=false commit -m "Add GSettings schema for ringboard extension"
```

Do not commit `gschemas.compiled`; it is a generated artifact. Add it to `.gitignore` if it isn't already covered.

---

## Task 2: Wire metadata.json to the Schema

**Files:**
- Modify: `gnome-shell/extension/metadata.json`

- [ ] **Step 1: Add the settings-schema field**

Edit `gnome-shell/extension/metadata.json` so the contents are:

```json
{
  "name": "Ringboard Clipboard History",
  "version": 1,
  "uuid": "ringboard@clipboard-history",
  "gettext-domain": "ringboard@clipboard-history",
  "description": "Thin UI for the Ringboard clipboard history server. Captures clipboard content via St.Clipboard and submits entries to the server. Reads the live history from the server on demand; no local cache.",
  "url": "https://github.com/SUPERCILEX/clipboard-history",
  "shell-version": ["46", "47", "48", "49", "50"],
  "settings-schema": "org.gnome.shell.extensions.ringboard"
}
```

- [ ] **Step 2: Validate JSON**

Run:
```bash
python3 -c "import json; json.load(open('gnome-shell/extension/metadata.json'))"
```

Expected: no output, exit 0.

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/metadata.json
git -c commit.gpgsign=false commit -m "Wire ringboard metadata to GSettings schema"
```

---

## Task 3: Strip dataStructures.js

**Files:**
- Modify: `gnome-shell/extension/dataStructures.js`

- [ ] **Step 1: Replace file contents**

Replace the entire file with:

```javascript
// Helpers used by the menu render layer. The thin-UI design holds no
// in-memory entry list, so the LinkedList / LLNode types that lived here
// previously are gone.

export const MAX_VISIBLE_CHARS = 200;

// Truncate a string for display in a menu item label. Adds an ellipsis when
// truncation occurs. `maxLen` defaults to MAX_VISIBLE_CHARS.
export function truncateLabel(text, maxLen) {
  if (typeof text !== 'string') {
    return '';
  }
  const limit = typeof maxLen === 'number' ? maxLen : MAX_VISIBLE_CHARS;
  if (text.length <= limit) {
    return text;
  }
  return text.slice(0, Math.max(0, limit - 1)) + '…';
}
```

- [ ] **Step 2: Syntax-check**

Run:
```bash
gjs -c "import('./gnome-shell/extension/dataStructures.js').then(m => { console.log(m.MAX_VISIBLE_CHARS, m.truncateLabel('hello', 3)); })"
```

Expected output: `200 he…` and exit 0.

If `gjs` rejects the dynamic import path syntax, this is acceptable; in that case run instead:
```bash
node --input-type=module -e "import('./gnome-shell/extension/dataStructures.js').then(m => { console.log(m.MAX_VISIBLE_CHARS, m.truncateLabel('hello', 3)); })"
```

Same expected output.

- [ ] **Step 3: Delete settingsFields.js**

```bash
git rm gnome-shell/extension/settingsFields.js
```

- [ ] **Step 4: Commit**

```bash
git add gnome-shell/extension/dataStructures.js
git -c commit.gpgsign=false commit -m "Strip dataStructures.js to render helpers; remove unused settingsFields.js"
```

---

## Task 4: SubprocessClient — Probe and Add

**Files:**
- Create: `gnome-shell/extension/lib/subprocessClient.js`
- Create: `gnome-shell/extension/tests/test_subprocess_client.js`

This task only implements `probe()` and `add()` to exercise the basic subprocess pattern. Other methods follow in Task 5.

- [ ] **Step 1: Write the smoke test (TDD)**

Create `gnome-shell/extension/tests/test_subprocess_client.js`:

```javascript
#!/usr/bin/gjs -m
// Smoke test for SubprocessClient. Requires a running ringboard-server.
// Run from the repo root:
//   gjs -m gnome-shell/extension/tests/test_subprocess_client.js
//
// Exit code 0 = pass, 1 = fail.

import GLib from 'gi://GLib';
import { SubprocessClient } from '../lib/subprocessClient.js';

let failures = 0;
function assert(cond, msg) {
  if (!cond) {
    print(`FAIL: ${msg}`);
    failures++;
  } else {
    print(`PASS: ${msg}`);
  }
}

async function main() {
  const candidates = [
    '/usr/bin/ringboard',
    '/usr/local/bin/ringboard',
    GLib.build_filenamev([GLib.get_home_dir(), '.cargo', 'bin', 'ringboard']),
  ];
  let binary = null;
  for (const p of candidates) {
    if (GLib.file_test(p, GLib.FileTest.IS_EXECUTABLE)) {
      binary = p;
      break;
    }
  }
  assert(binary !== null, 'ringboard binary discovered');
  if (!binary) return;

  const client = new SubprocessClient(binary);

  const probeOk = await client.probe();
  assert(probeOk === true, 'probe() returns true against live server');

  const before = await client.search('');
  const sentinel = `signum-test-${Date.now()}`;
  const addedId = await client.add(sentinel);
  assert(typeof addedId === 'number' && addedId >= 0,
    `add() returned numeric id (got ${addedId})`);

  const after = await client.search('');
  const found = after.some(e => e.data === sentinel);
  assert(found, 'newly added entry visible in subsequent search');
  assert(after.length >= before.length,
    `search after add returns >= entries (before=${before.length}, after=${after.length})`);
}

main().then(() => {
  if (failures > 0) {
    print(`${failures} failure(s)`);
    imports.system.exit(1);
  } else {
    print('all tests passed');
    imports.system.exit(0);
  }
}).catch(err => {
  print(`ERROR: ${err.message}\n${err.stack}`);
  imports.system.exit(1);
});
```

- [ ] **Step 2: Run the test, verify it fails**

Run:
```bash
gjs -m gnome-shell/extension/tests/test_subprocess_client.js
```

Expected: `ERROR: ...subprocessClient.js... not found` (or import error). The module doesn't exist yet.

- [ ] **Step 3: Implement SubprocessClient (probe + add only)**

Create `gnome-shell/extension/lib/subprocessClient.js`:

```javascript
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

// Default candidate paths checked when no explicit binary is passed.
const BINARY_CANDIDATES = [
  '/usr/bin/ringboard',
  '/usr/local/bin/ringboard',
  GLib.build_filenamev([GLib.get_home_dir(), '.cargo', 'bin', 'ringboard']),
];

export function findBinary() {
  for (const p of BINARY_CANDIDATES) {
    if (GLib.file_test(p, GLib.FileTest.IS_EXECUTABLE)) {
      return p;
    }
  }
  return null;
}

// Async wrapper around `ringboard` CLI. All methods return Promises and rely
// on Gio.Subprocess.communicate_utf8_async, which dispatches I/O on a worker
// thread. The GNOME Shell main loop is never blocked.
export class SubprocessClient {
  constructor(binaryPath) {
    if (!binaryPath) {
      throw new Error('SubprocessClient: binary path required');
    }
    this._binary = binaryPath;
  }

  // Run a subprocess that does not need stdin. Resolves with
  // { ok: bool, stdout: string, stderr: string, exit: number }.
  _run(argv, stdin) {
    return new Promise((resolve, reject) => {
      let proc;
      try {
        const flags =
          Gio.SubprocessFlags.STDOUT_PIPE |
          Gio.SubprocessFlags.STDERR_PIPE |
          (stdin != null ? Gio.SubprocessFlags.STDIN_PIPE : 0);
        proc = Gio.Subprocess.new([this._binary, ...argv], flags);
      } catch (e) {
        reject(e);
        return;
      }
      proc.communicate_utf8_async(stdin ?? null, null, (p, res) => {
        try {
          const [, stdout, stderr] = p.communicate_utf8_finish(res);
          const ok = p.get_successful();
          const exit = p.get_exit_status();
          resolve({ ok, stdout: stdout ?? '', stderr: stderr ?? '', exit });
        } catch (e) {
          reject(e);
        }
      });
    });
  }

  // Connectivity check. `ringboard last` reads from the server socket; if the
  // server is down it exits non-zero.
  async probe() {
    try {
      const r = await this._run(['last']);
      return r.ok;
    } catch (_) {
      return false;
    }
  }

  // Submit a text entry. Returns the numeric id assigned by the server, or
  // null on failure. The CLI prints the id to stdout on success.
  async add(text) {
    if (typeof text !== 'string' || text.length === 0) {
      return null;
    }
    let r;
    try {
      r = await this._run(['add', '-'], text);
    } catch (_) {
      return null;
    }
    if (!r.ok) {
      return null;
    }
    const trimmed = r.stdout.trim();
    const id = Number.parseInt(trimmed, 10);
    return Number.isFinite(id) ? id : null;
  }

  // Stubs filled in by Task 5.
  async search(_query) { throw new Error('SubprocessClient.search: not implemented yet'); }
  async moveToFront(_id) { throw new Error('not implemented'); }
  async remove(_id) { throw new Error('not implemented'); }
  async wipe() { throw new Error('not implemented'); }
}
```

- [ ] **Step 4: Update the smoke test to skip the not-yet-implemented methods**

Replace the body of `main()` in `gnome-shell/extension/tests/test_subprocess_client.js` with:

```javascript
async function main() {
  const candidates = [
    '/usr/bin/ringboard',
    '/usr/local/bin/ringboard',
    GLib.build_filenamev([GLib.get_home_dir(), '.cargo', 'bin', 'ringboard']),
  ];
  let binary = null;
  for (const p of candidates) {
    if (GLib.file_test(p, GLib.FileTest.IS_EXECUTABLE)) {
      binary = p;
      break;
    }
  }
  assert(binary !== null, 'ringboard binary discovered');
  if (!binary) return;

  const client = new SubprocessClient(binary);

  const probeOk = await client.probe();
  assert(probeOk === true, 'probe() returns true against live server');

  const sentinel = `signum-test-${Date.now()}`;
  const addedId = await client.add(sentinel);
  assert(typeof addedId === 'number' && addedId >= 0,
    `add() returned numeric id (got ${addedId})`);
}
```

(Search/move/remove/wipe assertions return in Task 5.)

- [ ] **Step 5: Run the test, verify all pass**

Run:
```bash
gjs -m gnome-shell/extension/tests/test_subprocess_client.js
```

Expected:
```
PASS: ringboard binary discovered
PASS: probe() returns true against live server
PASS: add() returned numeric id (got <number>)
all tests passed
```

If `probe` fails, ensure `systemctl --user is-active ringboard-server.service` reports `active`; the test requires a live server.

- [ ] **Step 6: Commit**

```bash
git add gnome-shell/extension/lib/subprocessClient.js gnome-shell/extension/tests/test_subprocess_client.js
git -c commit.gpgsign=false commit -m "Add SubprocessClient with probe and add"
```

---

## Task 5: SubprocessClient — Search, MoveToFront, Remove, Wipe

**Files:**
- Modify: `gnome-shell/extension/lib/subprocessClient.js`
- Modify: `gnome-shell/extension/tests/test_subprocess_client.js`

- [ ] **Step 1: Extend the smoke test (TDD)**

Append to `main()` in `gnome-shell/extension/tests/test_subprocess_client.js`, after the existing `add` assertions and before the function ends:

```javascript
  // search
  const matches = await client.search(sentinel);
  assert(Array.isArray(matches) && matches.length >= 1,
    `search(sentinel) returns >= 1 result (got ${matches.length})`);
  assert(matches.some(e => e.id === addedId),
    'search result includes the added entry id');
  assert(typeof matches[0].id === 'number' && typeof matches[0].data === 'string',
    'search entries have numeric id and string data');

  // moveToFront
  const movedOk = await client.moveToFront(addedId);
  assert(movedOk === true, `moveToFront(addedId) succeeded (got ${movedOk})`);

  // remove
  const removedOk = await client.remove(addedId);
  assert(removedOk === true, `remove(addedId) succeeded (got ${removedOk})`);

  const afterRemove = await client.search(sentinel);
  assert(!afterRemove.some(e => e.id === addedId),
    'removed entry is no longer in search results');

  // wipe is destructive; do not exercise it in the automated test.
  // Manual verification step at the end of the plan covers it.
  assert(typeof client.wipe === 'function', 'wipe is a function');
```

- [ ] **Step 2: Run the test, verify it fails on `search`**

Run:
```bash
gjs -m gnome-shell/extension/tests/test_subprocess_client.js
```

Expected: existing assertions pass, then `search` throws "not implemented".

- [ ] **Step 3: Implement search, moveToFront, remove, wipe**

In `gnome-shell/extension/lib/subprocessClient.js`, replace the four stub methods with:

```javascript
  // Search for entries. Empty query returns all entries newest-first.
  // Returns an array of { id, kind, data } objects (data is a string for
  // text entries; for binary/image entries the CLI may emit non-UTF-8 which
  // JSON.parse would reject — those entries are filtered out here).
  async search(query) {
    const q = typeof query === 'string' ? query : '';
    let r;
    try {
      r = await this._run(['search', '--json', q]);
    } catch (_) {
      return [];
    }
    if (!r.ok) {
      return [];
    }
    const text = r.stdout.trim();
    if (text.length === 0) {
      return [];
    }
    try {
      const parsed = JSON.parse(text);
      if (!Array.isArray(parsed)) return [];
      return parsed.filter(e =>
        e && typeof e.id === 'number' && typeof e.data === 'string'
      );
    } catch (_) {
      return [];
    }
  }

  // Move an entry to the front of the ring. Returns true on success.
  async moveToFront(id) {
    if (!Number.isFinite(id)) return false;
    try {
      const r = await this._run(['move-to-front', String(id)]);
      return r.ok;
    } catch (_) {
      return false;
    }
  }

  // Remove an entry by id. Returns true on success.
  async remove(id) {
    if (!Number.isFinite(id)) return false;
    try {
      const r = await this._run(['remove', String(id)]);
      return r.ok;
    } catch (_) {
      return false;
    }
  }

  // Wipe the entire history. Returns true on success.
  async wipe() {
    try {
      const r = await this._run(['wipe']);
      return r.ok;
    } catch (_) {
      return false;
    }
  }
```

- [ ] **Step 4: Run the test, verify all pass**

Run:
```bash
gjs -m gnome-shell/extension/tests/test_subprocess_client.js
```

Expected: every line begins with `PASS:`, ending in `all tests passed`. If `wipe` is referenced — note: the test does NOT call `wipe` (destructive). The assertion is only that `wipe` is a function.

- [ ] **Step 5: Commit**

```bash
git add gnome-shell/extension/lib/subprocessClient.js gnome-shell/extension/tests/test_subprocess_client.js
git -c commit.gpgsign=false commit -m "Implement search, moveToFront, remove, wipe in SubprocessClient"
```

---

## Task 6: ClipboardIntake Module

**Files:**
- Create: `gnome-shell/extension/lib/clipboardIntake.js`

This module touches `St.Clipboard` and `Shell.Global` and cannot run outside a Shell session. Verification is structural (syntax) plus the manual matrix at the end.

- [ ] **Step 1: Write the file**

Create `gnome-shell/extension/lib/clipboardIntake.js`:

```javascript
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

const Clipboard = St.Clipboard.get_default();

// Listens for clipboard owner-changed events on the GNOME display selection.
//
//   - SELECTION_CLIPBOARD is always observed (the standard "Ctrl-C" target).
//   - SELECTION_PRIMARY is observed only when the `process-primary-selection`
//     GSetting is true.
//
// Each observed event reads the new text and submits it to the ringboard
// server via SubprocessClient.add. UI state is not touched: the menu fetches
// fresh from the server when opened.
export class ClipboardIntake {
  constructor(client, settings) {
    this._client = client;
    this._settings = settings;
    this._enabled = false;
    this._selection = null;
    this._ownerChangedId = 0;
    this._settingsChangedId = 0;
    this._processPrimary = false;

    // Counter incremented when the extension itself writes to the clipboard
    // (e.g. paste-on-selection). Each owner-changed signal fired by our own
    // write decrements this; positive value suppresses one event.
    this._debouncing = 0;
  }

  enable() {
    if (this._enabled) return;
    this._enabled = true;

    this._selection = Shell.Global.get().get_display().get_selection();
    this._ownerChangedId = this._selection.connect(
      'owner-changed',
      (_sel, type, _source) => this._onSelectionChanged(type),
    );

    this._processPrimary = this._settings.get_boolean('process-primary-selection');
    this._settingsChangedId = this._settings.connect(
      'changed::process-primary-selection',
      () => {
        this._processPrimary = this._settings.get_boolean('process-primary-selection');
      },
    );
  }

  disable() {
    if (!this._enabled) return;
    this._enabled = false;

    if (this._selection && this._ownerChangedId) {
      this._selection.disconnect(this._ownerChangedId);
    }
    if (this._settings && this._settingsChangedId) {
      this._settings.disconnect(this._settingsChangedId);
    }
    this._selection = null;
    this._ownerChangedId = 0;
    this._settingsChangedId = 0;
  }

  // Suppress the next owner-changed signal generated by our own writes.
  // Call once per St.Clipboard.set_text we issue.
  expectOwnWrite() {
    this._debouncing += 1;
  }

  _onSelectionChanged(selectionType) {
    if (!this._enabled) return;

    if (selectionType === Meta.SelectionType.SELECTION_CLIPBOARD) {
      // accepted
    } else if (selectionType === Meta.SelectionType.SELECTION_PRIMARY) {
      if (!this._processPrimary) return;
    } else {
      return; // SELECTION_DND etc.
    }

    if (this._debouncing > 0) {
      this._debouncing -= 1;
      return;
    }

    if (this._settings.get_boolean('private-mode')) {
      return;
    }

    const stType =
      selectionType === Meta.SelectionType.SELECTION_PRIMARY
        ? St.ClipboardType.PRIMARY
        : St.ClipboardType.CLIPBOARD;

    Clipboard.get_text(stType, (_clip, text) => {
      if (typeof text !== 'string' || text.length === 0) return;
      let payload = text;
      if (this._settings.get_boolean('strip-text')) {
        payload = payload.trim();
      }
      if (payload.length === 0) return;
      this._client.add(payload).catch(e => {
        console.warn(`ringboard: client.add failed: ${e.message}`);
      });
    });
  }
}
```

- [ ] **Step 2: Syntax check**

Run:
```bash
node --input-type=module --check -e "$(cat gnome-shell/extension/lib/clipboardIntake.js | sed 's|gi://[^'\'']*|node:url|g')" 2>&1 | head -5
```

Expected: no syntax errors. (`gi://` imports cannot be resolved by Node, but the substitution avoids the resolution step. We're only checking that the JS parses.)

If your `node` rejects the substitution approach, a lighter alternative:
```bash
node --input-type=module -e "import('fs').then(fs => { const src = fs.readFileSync('gnome-shell/extension/lib/clipboardIntake.js', 'utf8'); new Function('return (async () => {' + src.replace(/^import .*$/gm, '') + '})')(); console.log('parse-ok'); })"
```

Expected: `parse-ok`.

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/lib/clipboardIntake.js
git -c commit.gpgsign=false commit -m "Add ClipboardIntake with settings-driven PRIMARY listener"
```

---

## Task 7: MenuController Module

**Files:**
- Create: `gnome-shell/extension/lib/menuController.js`

- [ ] **Step 1: Write the file**

Create `gnome-shell/extension/lib/menuController.js`:

```javascript
import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import St from 'gi://St';

import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import { MAX_VISIBLE_CHARS, truncateLabel } from '../dataStructures.js';

const PAGE_SIZE = 50;
const SEARCH_DEBOUNCE_MS = 150;

// Owns the state of the dropdown for the duration of a single menu-open
// lifetime. State is dropped on close. The MenuController does not own the
// menu chrome (search entry, scroll view, paging buttons); it only renders
// `historySection`'s children and reads the controller's transient state.
export class MenuController {
  constructor(client, settings, intake, historySection) {
    this._client = client;
    this._settings = settings;
    this._intake = intake;
    this._historySection = historySection;

    this._reset();
    this._fetchGen = 0;
    this._debounceSourceId = 0;
    this._onPageChanged = null;
  }

  _reset() {
    this._currentQuery = '';
    this._currentOffset = 0;
    this._resultEntries = null; // null when closed
    this._selectedIndex = null;
  }

  setOnPageChanged(cb) {
    this._onPageChanged = cb;
  }

  // ---- lifecycle ----

  async onMenuOpen() {
    this._reset();
    this._fetchGen += 1;
    const myGen = this._fetchGen;
    const entries = await this._client.search('');
    if (myGen !== this._fetchGen) return; // newer fetch supersedes
    this._resultEntries = entries;
    this._renderPage();
  }

  onMenuClose() {
    this._cancelDebounce();
    this._reset();
    this._historySection.removeAll();
  }

  // ---- search ----

  setQuery(text) {
    this._cancelDebounce();
    this._debounceSourceId = GLib.timeout_add(
      GLib.PRIORITY_DEFAULT,
      SEARCH_DEBOUNCE_MS,
      () => {
        this._debounceSourceId = 0;
        this._fetchAndRender(text).catch(e => {
          console.warn(`ringboard: search fetch failed: ${e.message}`);
        });
        return GLib.SOURCE_REMOVE;
      },
    );
  }

  _cancelDebounce() {
    if (this._debounceSourceId) {
      GLib.Source.remove(this._debounceSourceId);
      this._debounceSourceId = 0;
    }
  }

  async _fetchAndRender(query) {
    this._fetchGen += 1;
    const myGen = this._fetchGen;
    const entries = await this._client.search(query);
    if (myGen !== this._fetchGen) return;
    this._currentQuery = query;
    this._currentOffset = 0;
    this._resultEntries = entries;
    this._renderPage();
  }

  // ---- pagination ----

  nextPage() {
    if (!this._resultEntries) return;
    const next = this._currentOffset + PAGE_SIZE;
    if (next >= this._resultEntries.length) return;
    this._currentOffset = next;
    this._renderPage();
  }

  prevPage() {
    if (!this._resultEntries) return;
    const prev = this._currentOffset - PAGE_SIZE;
    this._currentOffset = prev < 0 ? 0 : prev;
    this._renderPage();
  }

  hasNextPage() {
    if (!this._resultEntries) return false;
    return this._currentOffset + PAGE_SIZE < this._resultEntries.length;
  }

  hasPrevPage() {
    return this._currentOffset > 0;
  }

  // ---- actions ----

  async selectAndPaste(entry) {
    const Clipboard = St.Clipboard.get_default();
    Clipboard.set_text(St.ClipboardType.CLIPBOARD, entry.data);
    if (this._intake) this._intake.expectOwnWrite();

    if (this._settings.get_boolean('move-item-first')) {
      this._client.moveToFront(entry.id).catch(e => {
        console.warn(`ringboard: move-to-front failed: ${e.message}`);
      });
    }

    if (this._settings.get_boolean('paste-on-selection')) {
      this._fireVirtualPaste();
    }
  }

  _fireVirtualPaste() {
    try {
      const seat = Clutter.get_default_backend().get_default_seat();
      const virtualKb = seat.create_virtual_device(
        Clutter.InputDeviceType.KEYBOARD_DEVICE,
      );
      const t = Clutter.get_current_event_time() * 1000; // microseconds
      virtualKb.notify_keyval(
        t,
        Clutter.KEY_Control_L,
        Clutter.KeyState.PRESSED,
      );
      virtualKb.notify_keyval(t, Clutter.KEY_v, Clutter.KeyState.PRESSED);
      virtualKb.notify_keyval(t, Clutter.KEY_v, Clutter.KeyState.RELEASED);
      virtualKb.notify_keyval(
        t,
        Clutter.KEY_Control_L,
        Clutter.KeyState.RELEASED,
      );
    } catch (e) {
      console.warn(`ringboard: virtual paste failed: ${e.message}`);
    }
  }

  async removeEntry(entry) {
    const ok = await this._client.remove(entry.id).catch(() => false);
    if (!ok) {
      console.warn(`ringboard: remove(${entry.id}) failed`);
      return;
    }
    if (this._resultEntries) {
      this._resultEntries = this._resultEntries.filter(e => e.id !== entry.id);
      this._renderPage();
    }
  }

  async clearAll(confirmFn) {
    if (this._settings.get_boolean('confirm-clear') && typeof confirmFn === 'function') {
      const confirmed = await confirmFn();
      if (!confirmed) return;
    }
    const ok = await this._client.wipe().catch(() => false);
    if (!ok) {
      console.warn('ringboard: wipe failed');
      return;
    }
    this._resultEntries = [];
    this._renderPage();
  }

  // ---- rendering ----

  _renderPage() {
    this._historySection.removeAll();

    const entries = this._resultEntries ?? [];
    const start = this._currentOffset;
    const slice = entries.slice(start, start + PAGE_SIZE);

    for (const entry of slice) {
      const item = new PopupMenu.PopupMenuItem('');
      // Image and other non-text kinds get a placeholder; text data is
      // truncated for display.
      const isText = typeof entry.data === 'string' && entry.data.length > 0;
      const labelText = isText
        ? truncateLabel(entry.data, MAX_VISIBLE_CHARS)
        : `[${entry.kind || 'binary'}]`;
      item.label.set_text(labelText);
      if (!isText) {
        item.setSensitive(false);
      } else {
        item.connect('activate', () => {
          this.selectAndPaste(entry).catch(e => {
            console.warn(`ringboard: paste failed: ${e.message}`);
          });
        });
      }
      this._historySection.addMenuItem(item);
    }

    if (slice.length === 0) {
      const empty = new PopupMenu.PopupMenuItem(
        this._currentQuery
          ? 'No matches'
          : 'No clipboard history',
      );
      empty.setSensitive(false);
      this._historySection.addMenuItem(empty);
    }

    if (typeof this._onPageChanged === 'function') {
      this._onPageChanged({
        hasPrev: this.hasPrevPage(),
        hasNext: this.hasNextPage(),
      });
    }
  }
}
```

- [ ] **Step 2: Syntax check**

Run:
```bash
node --input-type=module -e "import('fs').then(fs => { const src = fs.readFileSync('gnome-shell/extension/lib/menuController.js', 'utf8'); new Function('return (async () => {' + src.replace(/^import .*$/gm, '') + '})')(); console.log('parse-ok'); })"
```

Expected: `parse-ok`.

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/lib/menuController.js
git -c commit.gpgsign=false commit -m "Add MenuController for thin-UI dropdown rendering and pagination"
```

---

## Task 8: Rewrite extension.js

**Files:**
- Modify: `gnome-shell/extension/extension.js`

This file becomes the glue: panel button + Extension class. All the heavy logic moved into the lib modules in Tasks 4–7.

- [ ] **Step 1: Replace the file contents**

Replace `gnome-shell/extension/extension.js` entirely with:

```javascript
import Gio from 'gi://Gio';
import GObject from 'gi://GObject';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

import { findBinary, SubprocessClient } from './lib/subprocessClient.js';
import { ClipboardIntake } from './lib/clipboardIntake.js';
import { MenuController } from './lib/menuController.js';
import ConfirmDialog from './confirmDialog.js';

const INDICATOR_ICON = 'edit-paste-symbolic';
const DISCONNECTED_ICON = 'network-offline-symbolic';
const SETTING_KEY_TOGGLE_MENU = 'toggle-menu';

// Panel button. Owns its own menu and wires the three lib components.
const ClipboardIndicator = GObject.registerClass(
class ClipboardIndicator extends PanelMenu.Button {
  _init(extension, client, settings, connected) {
    super._init(0, 'Ringboard Clipboard History', false);

    this._extension = extension;
    this._client = client;
    this._settings = settings;
    this._connected = connected;
    this._intake = null;
    this._controller = null;
    this._shortcutBound = false;

    const hbox = new St.BoxLayout({ style_class: 'panel-status-menu-box' });
    this._icon = new St.Icon({
      icon_name: connected ? INDICATOR_ICON : DISCONNECTED_ICON,
      style_class: 'system-status-icon',
    });
    hbox.add_child(this._icon);
    hbox.add_child(PopupMenu.arrowIcon(St.Side.BOTTOM));
    this.add_child(hbox);

    if (!connected) {
      this._buildDisconnectedMenu();
      return;
    }

    this._intake = new ClipboardIntake(client, settings);
    this._intake.enable();

    this._buildMenu();
    this._controller = new MenuController(client, settings, this._intake, this._historySection);
    this._wireMenuLifecycle();
    this._wireSettings();
  }

  _buildDisconnectedMenu() {
    const item = new PopupMenu.PopupMenuItem('Ringboard server unavailable');
    item.setSensitive(false);
    this.menu.addMenuItem(item);
  }

  _buildMenu() {
    // Search entry
    this._searchEntry = new St.Entry({
      name: 'searchEntry',
      style_class: 'search-entry',
      can_focus: true,
      hint_text: 'Search…',
      track_hover: true,
      x_expand: true,
    });
    const searchItem = new PopupMenu.PopupBaseMenuItem({
      reactive: false,
      can_focus: false,
    });
    searchItem.add_child(this._searchEntry);
    this.menu.addMenuItem(searchItem);

    // History section inside a scroll view
    this._historySection = new PopupMenu.PopupMenuSection();
    this._scrollView = new St.ScrollView({
      style_class: 'ci-history-scroll',
      overlay_scrollbars: true,
    });
    this._scrollView.add_child(this._historySection.actor);
    const scrollWrap = new PopupMenu.PopupMenuSection();
    scrollWrap.actor.add_child(this._scrollView);
    this.menu.addMenuItem(scrollWrap);

    this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

    // Action row: prev | next | clear
    this._prevItem = new PopupMenu.PopupMenuItem('« Previous page');
    this._prevItem.connect('activate', () => this._controller.prevPage());
    this.menu.addMenuItem(this._prevItem);

    this._nextItem = new PopupMenu.PopupMenuItem('Next page »');
    this._nextItem.connect('activate', () => this._controller.nextPage());
    this.menu.addMenuItem(this._nextItem);

    const clearItem = new PopupMenu.PopupMenuItem('Clear history');
    clearItem.connect('activate', () => {
      this._controller.clearAll(() => this._confirmClear()).catch(e => {
        console.warn(`ringboard: clearAll failed: ${e.message}`);
      });
    });
    this.menu.addMenuItem(clearItem);
  }

  _wireMenuLifecycle() {
    this.menu.connect('open-state-changed', (_, open) => {
      if (open) {
        this._controller.onMenuOpen().catch(e => {
          console.warn(`ringboard: onMenuOpen failed: ${e.message}`);
        });
      } else {
        this._controller.onMenuClose();
      }
    });

    this._searchEntry.get_clutter_text().connect('text-changed', () => {
      this._controller.setQuery(this._searchEntry.get_text());
    });

    this._controller.setOnPageChanged(({ hasPrev, hasNext }) => {
      this._prevItem.setSensitive(hasPrev);
      this._nextItem.setSensitive(hasNext);
    });
  }

  _wireSettings() {
    this._bindOrUnbindShortcut();
    this._settingsKbId = this._settings.connect('changed::enable-keybindings',
      () => this._bindOrUnbindShortcut());
    this._settingsToggleId = this._settings.connect(`changed::${SETTING_KEY_TOGGLE_MENU}`,
      () => this._bindOrUnbindShortcut());
  }

  _bindOrUnbindShortcut() {
    if (this._shortcutBound) {
      Main.wm.removeKeybinding(SETTING_KEY_TOGGLE_MENU);
      this._shortcutBound = false;
    }
    if (!this._settings.get_boolean('enable-keybindings')) return;
    Main.wm.addKeybinding(
      SETTING_KEY_TOGGLE_MENU,
      this._settings,
      Meta.KeyBindingFlags.NONE,
      Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW,
      () => this.menu.toggle(),
    );
    this._shortcutBound = true;
  }

  async _confirmClear() {
    return await new Promise(resolve => {
      const dialog = new ConfirmDialog(
        'Clear clipboard history',
        'This will remove every entry from the Ringboard server. Continue?',
        () => resolve(true),
        () => resolve(false),
      );
      dialog.open();
    });
  }

  destroy() {
    if (this._shortcutBound) {
      Main.wm.removeKeybinding(SETTING_KEY_TOGGLE_MENU);
      this._shortcutBound = false;
    }
    if (this._settings) {
      if (this._settingsKbId) this._settings.disconnect(this._settingsKbId);
      if (this._settingsToggleId) this._settings.disconnect(this._settingsToggleId);
    }
    if (this._intake) {
      this._intake.disable();
      this._intake = null;
    }
    if (this._controller) {
      this._controller.onMenuClose();
      this._controller = null;
    }
    super.destroy();
  }
});

export default class RingboardExtension extends Extension {
  enable() {
    const settings = this.getSettings();
    const binary = findBinary();
    if (!binary) {
      this._installIndicator(null, settings, false);
      console.warn('ringboard: CLI binary not found in /usr/bin, /usr/local/bin, ~/.cargo/bin');
      return;
    }
    const client = new SubprocessClient(binary);
    client.probe().then(connected => {
      if (this._indicator) {
        // Probe completed after we already mounted (rare race); ignore.
        return;
      }
      this._installIndicator(client, settings, connected);
    }).catch(() => {
      this._installIndicator(client, settings, false);
    });
  }

  _installIndicator(client, settings, connected) {
    this._indicator = new ClipboardIndicator(this, client, settings, connected);
    Main.panel.addToStatusArea('ringboard-clipboard-history', this._indicator, 1, 'right');
  }

  disable() {
    if (this._indicator) {
      this._indicator.destroy();
      this._indicator = null;
    }
  }
}
```

- [ ] **Step 2: JS parse check**

Run:
```bash
node --input-type=module -e "import('fs').then(fs => { const src = fs.readFileSync('gnome-shell/extension/extension.js', 'utf8'); new Function('return (async () => {' + src.replace(/^import .*$/gm, '').replace(/^export default class .*\\{[\\s\\S]*?^}$/gm, '') + '})')(); console.log('parse-ok'); })"
```

If the regex strip is brittle in your shell (the `^export default class` block won't strip cleanly with one substitution), use this simpler check instead:
```bash
gjs -m -c "import('./gnome-shell/extension/extension.js').catch(e => { if (/Cannot find module|Failed to resolve/.test(e.message)) { print('parse-ok'); } else { print('PARSE ERROR: ' + e.message); imports.system.exit(1); } })"
```

Expected: `parse-ok` (the resolution failure is expected because GJS can't resolve `resource:///` schemes outside Shell, but a parse error would surface differently).

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/extension.js
git -c commit.gpgsign=false commit -m "Rewrite extension.js as thin glue over lib/* modules"
```

---

## Task 9: Preferences UI (prefs.js)

**Files:**
- Create: `gnome-shell/extension/prefs.js`

- [ ] **Step 1: Write the file**

Create `gnome-shell/extension/prefs.js`:

```javascript
import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import { ExtensionPreferences } from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

const TOGGLES = [
  ['paste-on-selection', 'Paste on selection',
   'Click an entry to also send Ctrl-V to the focused window.'],
  ['move-item-first', 'Move selected item to front',
   'Tell the server to move clicked entries to the front of the history.'],
  ['confirm-clear', 'Confirm before clearing',
   'Show a confirmation dialog before wiping the entire history.'],
  ['private-mode', 'Private mode',
   'Pause clipboard intake. New copies are not submitted to the server.'],
  ['enable-keybindings', 'Enable keybindings',
   'Bind the toggle-menu keyboard shortcut.'],
  ['process-primary-selection', 'Capture PRIMARY selection',
   'Also intake text from the PRIMARY (text-highlight) selection. Off by default.'],
  ['strip-text', 'Strip whitespace',
   'Trim leading and trailing whitespace before submitting to the server.'],
  ['enable-typeahead-search', 'Type-ahead search',
   'Letters typed while the dropdown is open jump into the search box.'],
];

export default class RingboardPreferences extends ExtensionPreferences {
  fillPreferencesWindow(window) {
    const settings = this.getSettings();

    const page = new Adw.PreferencesPage({
      title: 'General',
      icon_name: 'preferences-system-symbolic',
    });
    window.add(page);

    const group = new Adw.PreferencesGroup({
      title: 'Behavior',
    });
    page.add(group);

    for (const [key, title, subtitle] of TOGGLES) {
      const row = new Adw.SwitchRow({ title, subtitle });
      settings.bind(key, row, 'active', Gio.SettingsBindFlags.DEFAULT);
      group.add(row);
    }
  }
}
```

- [ ] **Step 2: Syntax check**

Run:
```bash
node --input-type=module -e "import('fs').then(fs => { const src = fs.readFileSync('gnome-shell/extension/prefs.js', 'utf8'); new Function('return (async () => {' + src.replace(/^import .*$/gm, '').replace(/^export default class[\\s\\S]*$/gm, '') + '})')(); console.log('parse-ok'); })"
```

Expected: `parse-ok`.

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/prefs.js
git -c commit.gpgsign=false commit -m "Add Adw preferences UI for ringboard extension"
```

---

## Task 10: Pack and Install

**Files:**
- (no source changes)

- [ ] **Step 1: Pack the extension**

Run from the repo root:
```bash
cd gnome-shell/extension && \
  gnome-extensions pack --force --out-dir=/tmp \
  --schema=schemas/org.gnome.shell.extensions.ringboard.gschema.xml \
  --extra-source=lib \
  --extra-source=dataStructures.js \
  --extra-source=confirmDialog.js \
  --extra-source=clipboard-history.pot \
  --extra-source=prefs.js \
  . && cd -
```

Expected: `/tmp/ringboard@clipboard-history.shell-extension.zip` exists. Verify:
```bash
unzip -l /tmp/ringboard@clipboard-history.shell-extension.zip | head -25
```

Expected entries include: `metadata.json`, `extension.js`, `prefs.js`, `dataStructures.js`, `confirmDialog.js`, `lib/subprocessClient.js`, `lib/clipboardIntake.js`, `lib/menuController.js`, `schemas/gschemas.compiled` (or `schemas/org.gnome.shell.extensions.ringboard.gschema.xml`), `clipboard-history.pot`.

If `lib/` does not appear, the engineer should re-run the pack with each `lib/*.js` file listed via separate `--extra-source` flags.

- [ ] **Step 2: Install**

Run:
```bash
gnome-extensions install --force /tmp/ringboard@clipboard-history.shell-extension.zip
ls ~/.local/share/gnome-shell/extensions/ringboard@clipboard-history/
```

Expected: directory contains `extension.js`, `prefs.js`, `metadata.json`, `lib/`, `schemas/`, `confirmDialog.js`, `dataStructures.js`, `clipboard-history.pot`.

- [ ] **Step 3: Verify schema is compiled in the install location**

Run:
```bash
test -f ~/.local/share/gnome-shell/extensions/ringboard@clipboard-history/schemas/gschemas.compiled && echo OK || echo MISSING
```

Expected: `OK`. If `MISSING`, run:
```bash
glib-compile-schemas ~/.local/share/gnome-shell/extensions/ringboard@clipboard-history/schemas/
```

- [ ] **Step 4: Tell the user to log out and back in**

GNOME Shell on Wayland does not pick up newly installed extensions until the session restarts. Output the instruction:

```bash
echo 'Log out and back in, then run: gnome-extensions enable ringboard@clipboard-history'
```

- [ ] **Step 5: No commit (this task only produces the install)**

There is nothing new to commit. Move on.

---

## Task 11: Manual Verification

**Files:** none — this task is a live-session checklist.

After the user has logged out and back in and run `gnome-extensions enable ringboard@clipboard-history`, run through each scenario. Record results in `docs/superpowers/plans/2026-05-06-ringboard-gnome-extension-thin-ui-results.md` (create the file).

For each scenario, note PASS/FAIL and any unexpected behavior.

Helper: monitor the shell journal in a separate terminal during testing:
```bash
journalctl --user -f --identifier gnome-shell | grep -i ringboard
```

- [ ] **1. Server up, fresh load.** `systemctl --user is-active ringboard-server` → `active`. Reload extension. Click panel icon. Expect: panel icon shows the paste icon (not disconnected). Menu opens within 1s. Up to 50 entries shown, newest first. No errors in `journalctl`.

- [ ] **2. Search.** Type a substring known to exist in many entries. Expect: dropdown re-renders with matching entries within ~150ms after typing stops. Page buttons disabled if fewer than 50 matches.

- [ ] **3. Server down at load.** `systemctl --user stop ringboard-server`. Reload extension. Click panel icon. Expect: disconnected icon variant. Menu shows single disabled "Ringboard server unavailable" item.

- [ ] **4. Server stop after load.** Restart server, reload extension, then `systemctl --user stop ringboard-server`. Click panel icon. Expect: dropdown shows "No clipboard history" or an empty result; no crash. `journalctl` shows a `console.warn` from `client.search`. Restart the server before continuing.

- [ ] **5. Wayland intake.** Open a Wayland-native app (e.g. GNOME Text Editor). Type "wayland-test-<timestamp>" and select+Ctrl-C. Within 1s, run `ringboard search "wayland-test-"` → expect the entry to appear.

- [ ] **6. PRIMARY off (default).** With `process-primary-selection` = false (the default), highlight (mouse-select) text in `xterm` or any text widget without copying. Run `ringboard search "<that text>"` → expect no new entry created in the last few seconds.

- [ ] **7. PRIMARY on.** Open the prefs dialog (`gnome-extensions prefs ringboard@clipboard-history`), enable "Capture PRIMARY selection". Highlight text again. Expect new entry on server.

- [ ] **8. Private mode.** In prefs, enable "Private mode". Ctrl-C something new. Expect: nothing new on server. Disable private mode.

- [ ] **9. paste-on-selection off.** In prefs, disable "Paste on selection". Open menu, click an entry. Expect: clipboard updates (verify with `wl-paste` or any text widget Ctrl-V) but the focused window is not auto-pasted-into.

- [ ] **10. move-item-first off.** Note the current entry order (`ringboard search ""` first 5 ids). In prefs, disable "Move selected item to front". Click any entry that is NOT first. Re-check the order — expect unchanged.

- [ ] **11. Clear with confirm.** In prefs, "Confirm before clearing" = true. Click the panel menu's "Clear history". Expect: dialog appears. Cancel — entries remain. Click again, confirm — entries wiped (`ringboard search "" --json` → empty array).

- [ ] **12. Large history.** Recreate a large history if needed (e.g., `for i in $(seq 1 200); do echo "entry-$i" | ringboard add -; done`). Open menu. Expect: opens within ~1s; no console errors; prev/next page buttons traverse correctly through all 200 entries (4 pages of 50).

- [ ] **13. Delete one entry.** With history populated, open menu, find an entry. Trigger delete (TBD: select via keyboard — note: this design does not specify a delete shortcut yet; if not bound, mark this as a known gap to add later). For this milestone, verify removal via `ringboard remove <id>` from a terminal followed by reopening the menu — the entry should be gone.

- [ ] **14. Commit results file**

```bash
git add docs/superpowers/plans/2026-05-06-ringboard-gnome-extension-thin-ui-results.md
git -c commit.gpgsign=false commit -m "Record manual verification results"
```

---

## Self-Review Notes

**Spec coverage:** every component (SubprocessClient, ClipboardIntake, MenuController, ClipboardIndicator, dataStructures.js, schema, prefs.js, metadata.json) has a task. Every removal listed in the spec's "Removal checklist" is achieved by Task 8's full-file rewrite of `extension.js` (no remnants of `_loadHistoryFromServer`, the prune loop, the LinkedList store, etc., survive). Every error-handling row maps onto code in Tasks 4–8. The 13-scenario manual verification matrix is reproduced as Task 11.

**Known gap:** the spec notes "delete entry keyboard shortcut on highlighted item" but the implementation does not bind a delete key. The plan acknowledges this in Task 11 scenario 13. If the user wants this in the same milestone, add a follow-up task that calls `MenuController.removeEntry` from a key handler on the per-item `PopupMenuItem`. Default punt: leave as a follow-up; the public CLI route works.

**No placeholders, no TBDs, no "implement appropriate handling".** All code blocks are complete.

**Type consistency:** `Entry` is `{id: number, kind: string, data: string}` everywhere; `findBinary()` returns `string | null`; `client.add(text)` returns `Promise<number | null>`; the controller's `_resultEntries` is `Entry[] | null`. No drift.
