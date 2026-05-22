# Extension binary clipboard intake — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the GNOME extension capture all clipboard payloads (images, custom formats, etc.) — not just text — by porting `ringboard-wayland`'s `BestMimeTypeFinder` policy into the extension's intake path.

**Architecture:** On every `owner-changed`, the intake enumerates `St.Clipboard.get_mimetypes`, runs a JS port of the watcher's priority rules (`selectBestMime`), then reads bytes via `get_text` (plain-text slot) or `get_content` (everything else), and submits one entry through the DBus `Add(ay, s)` method which already exists server-side.

**Tech Stack:** GJS (GNOME Shell ESM modules), `St.Clipboard`, `Gio.DBus`. Tests for the pure-JS module run under Node 22+ (`node --test`).

**Branch:** `feat/extension-dbus-client` in worktree `/home/javier/projects/spinoffs/clipboard-history-extdbus`.

**Spec:** `docs/superpowers/specs/2026-05-22-extension-binary-clipboard-intake-design.md`.

---

## File structure

- **Create** `gnome-shell/extension/lib/mimePriority.js` — pure function `selectBestMime(mimes)`. No GJS imports. Sole owner of MIME-priority logic.
- **Create** `gnome-shell/extension/lib/mimePriority.test.mjs` — Node `node:test` suite covering the priority rules.
- **Create** `gnome-shell/extension/package.json` — `{"type":"module","private":true}` so Node treats `lib/*.js` as ESM during tests. GJS itself ignores `package.json`.
- **Modify** `gnome-shell/extension/lib/dbusClient.js` — `add(text: string)` → `add(payloadBytes: Uint8Array, mime: string)`. Internal-only API; intake is the sole caller.
- **Modify** `gnome-shell/extension/lib/clipboardIntake.js` — replace the text-only read in `_onSelectionChanged` with the enumerate-select-read flow. Generalise `expectOwnWrite` to accept binary payloads.
- **Modify** `gnome-shell/extension/lib/menuController.js` — in `selectAndPaste`, the binary branch passes `{mime, bytes}` to `expectOwnWrite` before `Clipboard.set_content`.

No server-side changes. No new DBus method.

---

## Task 1: Test scaffolding + first failing test

**Files:**
- Create: `gnome-shell/extension/package.json`
- Create: `gnome-shell/extension/lib/mimePriority.test.mjs`

- [ ] **Step 1: Add package.json so Node treats lib/*.js as ESM**

Create `gnome-shell/extension/package.json`:

```json
{
  "name": "ringboard-extension",
  "private": true,
  "type": "module"
}
```

- [ ] **Step 2: Write the first failing test**

Create `gnome-shell/extension/lib/mimePriority.test.mjs`:

```javascript
import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { selectBestMime } from './mimePriority.js';

test('empty input returns null', () => {
  assert.equal(selectBestMime([]), null);
});

test('non-array input returns null', () => {
  assert.equal(selectBestMime(null), null);
  assert.equal(selectBestMime(undefined), null);
});
```

- [ ] **Step 3: Run test, expect failure**

```bash
cd gnome-shell/extension && node --test lib/mimePriority.test.mjs
```

Expected: failure, `Error [ERR_MODULE_NOT_FOUND]: Cannot find module … mimePriority.js`.

- [ ] **Step 4: Commit scaffolding**

```bash
git add gnome-shell/extension/package.json gnome-shell/extension/lib/mimePriority.test.mjs
git commit -m "gnome-shell: scaffold node:test runner for extension lib"
```

---

## Task 2: `selectBestMime` — empty cases and plain-text slot

**Files:**
- Create: `gnome-shell/extension/lib/mimePriority.js`
- Modify: `gnome-shell/extension/lib/mimePriority.test.mjs`

- [ ] **Step 1: Implement the empty-input branches**

Create `gnome-shell/extension/lib/mimePriority.js`:

```javascript
// Mirrors `BestMimeTypeFinder` from
// client-sdk/src/watcher_utils/best_target.rs. Given the MIME types a
// clipboard source has advertised, return the single MIME we want to
// capture, or null if the offer should be dropped (passwords, unknown
// app-internal targets, empty).
//
// Slot priority (highest first):
//   0  plain text  (is_plaintext_mime: '', text, string, utf8_string,
//                   text/plain, text/plain;charset=*)
//   1  image/*
//   2  x-special/*
//   3  chromium/x-web-custom-data
//   4  any other text/*
//   5  anything else starting with an ASCII lowercase letter

const PLAIN_TEXT_ALIASES = new Set([
  '',
  'text',
  'string',
  'utf8_string',
  'text/plain',
  'text/plain;charset=utf-8',
  'text/plain;charset=us-ascii',
  'text/plain;charset=unicode',
]);

const SLOT_PLAIN = 0;
const SLOT_IMAGE = 1;
const SLOT_X_SPECIAL = 2;
const SLOT_CHROMIUM_CUSTOM = 3;
const SLOT_ANY_TEXT = 4;
const SLOT_OTHER = 5;
const NUM_SLOTS = 6;

function isPlaintextMime(mime) {
  return PLAIN_TEXT_ALIASES.has(mime.toLowerCase());
}

function startsWithLowercaseAscii(s) {
  if (s.length === 0) return true;
  const c = s.charCodeAt(0);
  return c >= 0x61 && c <= 0x7a; // 'a'..'z'
}

// Classify one MIME into a slot, or one of the sentinel strings:
//   'skip'     — ignore this MIME, keep processing others
//   'password' — privacy hint seen, drop the whole entry
function classify(mime) {
  if (isPlaintextMime(mime)) return SLOT_PLAIN;
  if (mime.startsWith('image/')) return SLOT_IMAGE;
  if (mime.startsWith('x-special/')) return SLOT_X_SPECIAL;
  if (mime === 'chromium/x-web-custom-data') return SLOT_CHROMIUM_CUSTOM;
  if (mime.startsWith('chromium/x-internal')) return 'skip';
  if (mime.startsWith('text/')) return SLOT_ANY_TEXT;
  if (mime === 'x-kde-passwordManagerHint') return 'password';
  if (startsWithLowercaseAscii(mime)) return SLOT_OTHER;
  return 'skip';
}

export function selectBestMime(mimes) {
  if (!Array.isArray(mimes)) return null;

  const slots = new Array(NUM_SLOTS).fill(null);
  let isPassword = false;

  for (const mime of mimes) {
    if (typeof mime !== 'string') continue;
    const cls = classify(mime);
    if (cls === 'skip') continue;
    if (cls === 'password') {
      isPassword = true;
      continue;
    }
    const current = slots[cls];
    if (current === null) {
      slots[cls] = mime;
    } else if (current.includes(';') && !mime.includes(';')) {
      // Prefer the MIME without parameters within the same slot.
      slots[cls] = mime;
    }
  }

  if (isPassword) return null;

  for (let s = 0; s < NUM_SLOTS; s++) {
    if (slots[s] !== null) {
      return { mime: slots[s], isText: s === SLOT_PLAIN };
    }
  }
  return null;
}
```

- [ ] **Step 2: Run scaffolding tests, expect pass**

```bash
cd gnome-shell/extension && node --test lib/mimePriority.test.mjs
```

Expected: 3 tests pass (empty + 2 non-array).

- [ ] **Step 3: Add plain-text slot tests**

Append to `gnome-shell/extension/lib/mimePriority.test.mjs`:

```javascript
test('text/plain only → plain text slot', () => {
  assert.deepEqual(selectBestMime(['text/plain']), { mime: 'text/plain', isText: true });
});

test('plain-text aliases (case-insensitive)', () => {
  for (const m of ['', 'TEXT', 'STRING', 'UTF8_STRING', 'text/plain;charset=utf-8']) {
    const got = selectBestMime([m]);
    assert.deepEqual(got, { mime: m, isText: true }, `failed for ${JSON.stringify(m)}`);
  }
});
```

- [ ] **Step 4: Run all tests, expect pass**

```bash
cd gnome-shell/extension && node --test lib/mimePriority.test.mjs
```

Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add gnome-shell/extension/lib/mimePriority.js gnome-shell/extension/lib/mimePriority.test.mjs
git commit -m "gnome-shell: selectBestMime — plain-text slot and aliases"
```

---

## Task 3: `selectBestMime` — image and x-special slots, priority ordering

**Files:**
- Modify: `gnome-shell/extension/lib/mimePriority.test.mjs`

- [ ] **Step 1: Add image and ordering tests**

Append to `gnome-shell/extension/lib/mimePriority.test.mjs`:

```javascript
test('image/png only → image slot, isText false', () => {
  assert.deepEqual(selectBestMime(['image/png']), { mime: 'image/png', isText: false });
});

test('plain text beats image when both are offered', () => {
  assert.deepEqual(
    selectBestMime(['image/png', 'text/plain']),
    { mime: 'text/plain', isText: true },
  );
});

test('image beats x-special', () => {
  assert.deepEqual(
    selectBestMime(['x-special/gnome-copied-files', 'image/png']),
    { mime: 'image/png', isText: false },
  );
});

test('x-special beats chromium custom', () => {
  assert.deepEqual(
    selectBestMime(['chromium/x-web-custom-data', 'x-special/foo']),
    { mime: 'x-special/foo', isText: false },
  );
});

test('any text/* falls into the any-text slot when no plain text', () => {
  assert.deepEqual(
    selectBestMime(['text/html']),
    { mime: 'text/html', isText: false },
  );
});

test('plain text beats text/html', () => {
  assert.deepEqual(
    selectBestMime(['text/html', 'text/plain']),
    { mime: 'text/plain', isText: true },
  );
});
```

- [ ] **Step 2: Run all tests, expect pass**

```bash
cd gnome-shell/extension && node --test lib/mimePriority.test.mjs
```

Expected: 11 tests pass. The implementation from Task 2 already covers these.

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/lib/mimePriority.test.mjs
git commit -m "gnome-shell: test selectBestMime priority ordering"
```

---

## Task 4: `selectBestMime` — filters (chromium-internal, uppercase, password)

**Files:**
- Modify: `gnome-shell/extension/lib/mimePriority.test.mjs`

- [ ] **Step 1: Add filter tests**

Append to `gnome-shell/extension/lib/mimePriority.test.mjs`:

```javascript
test('chromium/x-internal-* is filtered out', () => {
  assert.equal(selectBestMime(['chromium/x-internal-foo']), null);
});

test('uppercase-leading MIME is filtered out', () => {
  assert.equal(selectBestMime(['SomeAppFoo']), null);
});

test('uppercase filter does not affect plaintext aliases', () => {
  // STRING is a plain-text alias even though it starts uppercase.
  assert.deepEqual(selectBestMime(['STRING']), { mime: 'STRING', isText: true });
});

test('x-kde-passwordManagerHint alone returns null', () => {
  assert.equal(selectBestMime(['x-kde-passwordManagerHint']), null);
});

test('password hint suppresses an otherwise-valid offer', () => {
  assert.equal(
    selectBestMime(['image/png', 'text/plain', 'x-kde-passwordManagerHint']),
    null,
  );
});

test('only filtered MIMEs → null', () => {
  assert.equal(
    selectBestMime(['chromium/x-internal-a', 'SomeApp', 'OtherApp']),
    null,
  );
});
```

- [ ] **Step 2: Run all tests, expect pass**

```bash
cd gnome-shell/extension && node --test lib/mimePriority.test.mjs
```

Expected: 17 tests pass. Coverage already implemented in Task 2.

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/lib/mimePriority.test.mjs
git commit -m "gnome-shell: test selectBestMime filters (password, chromium, uppercase)"
```

---

## Task 5: `selectBestMime` — prefer-no-params and other-slot

**Files:**
- Modify: `gnome-shell/extension/lib/mimePriority.test.mjs`

- [ ] **Step 1: Add remaining tests**

Append to `gnome-shell/extension/lib/mimePriority.test.mjs`:

```javascript
test('prefer image without params over image with params', () => {
  // Order with params first, no-params second: should pick no-params.
  assert.deepEqual(
    selectBestMime(['image/png;qualifier=foo', 'image/png']),
    { mime: 'image/png', isText: false },
  );
});

test('keep the first image when both lack params', () => {
  assert.deepEqual(
    selectBestMime(['image/png', 'image/bmp']),
    { mime: 'image/png', isText: false },
  );
});

test('keep the first image when both have params', () => {
  assert.deepEqual(
    selectBestMime(['image/png;a=1', 'image/png;b=2']),
    { mime: 'image/png;a=1', isText: false },
  );
});

test('application/* lands in other slot', () => {
  assert.deepEqual(
    selectBestMime(['application/pdf']),
    { mime: 'application/pdf', isText: false },
  );
});

test('image beats application (other slot)', () => {
  assert.deepEqual(
    selectBestMime(['application/pdf', 'image/png']),
    { mime: 'image/png', isText: false },
  );
});

test('non-string entries are ignored', () => {
  assert.deepEqual(
    selectBestMime([null, 42, 'text/plain', undefined]),
    { mime: 'text/plain', isText: true },
  );
});
```

- [ ] **Step 2: Run all tests, expect pass**

```bash
cd gnome-shell/extension && node --test lib/mimePriority.test.mjs
```

Expected: 23 tests pass.

- [ ] **Step 3: Commit**

```bash
git add gnome-shell/extension/lib/mimePriority.test.mjs
git commit -m "gnome-shell: test selectBestMime prefer-no-params and other slot"
```

---

## Task 6: Switch `add` to `(bytes, mime)` and wire the new intake flow

Combined task. The `DbusClient.add` signature change and the `clipboardIntake.js` wiring must land in the same commit — splitting them would leave a commit where intake calls the old signature against the new method (or vice versa) and the extension stops capturing anything.

**Files:**
- Modify: `gnome-shell/extension/lib/dbusClient.js`
- Modify: `gnome-shell/extension/lib/clipboardIntake.js`

### Part A — dbusClient

- [ ] **Step 1: Replace `DbusClient.add`**

In `gnome-shell/extension/lib/dbusClient.js`, find the existing `add` method:

```javascript
  // Submit a text entry. Returns the numeric id assigned by the server, or
  // null on failure.
  async add(text) {
    if (typeof text !== 'string' || text.length === 0) return null;
    const bytes = TEXT_ENCODER.encode(text);
    try {
      const reply = await this._call(
        'Add',
        new GLib.Variant('(ays)', [bytes, 'text/plain;charset=utf-8']),
        new GLib.VariantType('(t)'),
      );
      const [id] = reply.deep_unpack();
      return Number(id);
    } catch (e) {
      console.warn(`ringboard: dbus Add failed: ${e.message}`);
      return null;
    }
  }
```

Replace it with:

```javascript
  // Submit an entry. Returns the numeric id assigned by the server, or
  // null on failure. `payloadBytes` is a Uint8Array; `mime` is the MIME
  // type to record (e.g. 'text/plain;charset=utf-8' or 'image/png').
  async add(payloadBytes, mime) {
    if (!(payloadBytes instanceof Uint8Array) || payloadBytes.length === 0) return null;
    if (typeof mime !== 'string' || mime.length === 0) return null;
    try {
      const reply = await this._call(
        'Add',
        new GLib.Variant('(ays)', [payloadBytes, mime]),
        new GLib.VariantType('(t)'),
      );
      const [id] = reply.deep_unpack();
      return Number(id);
    } catch (e) {
      console.warn(`ringboard: dbus Add failed: ${e.message}`);
      return null;
    }
  }
```

- [ ] **Step 2: Drop the now-unused `TEXT_ENCODER` declaration**

```bash
grep -n TEXT_ENCODER gnome-shell/extension/lib/dbusClient.js
```

Expected: only the top-of-file declaration line remains. Remove it:

```bash
sed -i '/^const TEXT_ENCODER = new TextEncoder();$/d' gnome-shell/extension/lib/dbusClient.js
```

Verify it's gone:

```bash
grep -n TEXT_ENCODER gnome-shell/extension/lib/dbusClient.js
```

Expected: no output.

### Part B — clipboardIntake

This part changes the read flow end to end. After this task the extension still works for text and additionally captures any non-text MIME that `selectBestMime` accepts.

- [ ] **Step 3: Add the import and update the top-of-file comment**

In `gnome-shell/extension/lib/clipboardIntake.js`, replace the import block and the file-level comment:

```javascript
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

const Clipboard = St.Clipboard.get_default();
```

with:

```javascript
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import { selectBestMime } from './mimePriority.js';

const Clipboard = St.Clipboard.get_default();

const TEXT_ENCODER = new TextEncoder();
const PLAIN_TEXT_MIME = 'text/plain;charset=utf-8';
```

Then update the top file comment so it reflects the new behavior. Replace:

```javascript
// Listens for clipboard owner-changed events on the GNOME display selection.
//
//   - SELECTION_CLIPBOARD is always observed (the standard "Ctrl-C" target).
//   - SELECTION_PRIMARY is observed only when the `process-primary-selection`
//     GSetting is true.
//
// Each observed event reads the new text and submits it to the ringboard
// server via DbusClient.add. UI state is not touched: the menu fetches fresh
// from the server when opened.
```

with:

```javascript
// Listens for clipboard owner-changed events on the GNOME display selection.
//
//   - SELECTION_CLIPBOARD is always observed (the standard "Ctrl-C" target).
//   - SELECTION_PRIMARY is observed only when the `process-primary-selection`
//     GSetting is true.
//
// For each observed event the intake enumerates the offered MIME types via
// St.Clipboard.get_mimetypes, picks one through selectBestMime (a JS port
// of ringboard-wayland's BestMimeTypeFinder), reads the bytes for that
// MIME, and submits a single entry to ringboard-server via DbusClient.add.
// UI state is not touched: the menu fetches fresh from the server when
// opened.
```

- [ ] **Step 4: Replace the `_lastSelfWrite` doc comment in the constructor**

Find:

```javascript
    // Content-match suppression: when the extension writes to the clipboard
    // itself, it records the payload and monotonic timestamp here. The next
    // owner-changed signal whose read matches this payload (within TTL) is
    // skipped. Counting signals is unreliable because set_text on identical
    // content doesn't always trigger owner-changed.
    this._lastSelfWrite = null; // { text: string, expiresUs: number }
```

Replace with:

```javascript
    // Content-match suppression: when the extension writes to the clipboard
    // itself, it records the MIME, payload, and monotonic timestamp here.
    // The next owner-changed signal whose read matches this MIME + payload
    // (within TTL) is skipped. Counting signals is unreliable because
    // set_text / set_content on identical content doesn't always trigger
    // owner-changed.
    //
    // `payload` is a string for text writes, Uint8Array for binary writes.
    this._lastSelfWrite = null; // { mime, payload: string|Uint8Array, expiresUs }
```

- [ ] **Step 5: Replace `expectOwnWrite`**

Find:

```javascript
  // Record an impending self-write so the matching owner-changed signal can
  // be skipped. `text` is the exact string we'll set on the clipboard, or
  // null for binary writes (which intake already ignores via get_text).
  expectOwnWrite(text) {
    if (typeof text !== 'string' || text.length === 0) {
      this._lastSelfWrite = null;
      return;
    }
    this._lastSelfWrite = {
      text,
      expiresUs: GLib.get_monotonic_time() + ClipboardIntake.SELF_WRITE_TTL_US,
    };
  }
```

Replace with:

```javascript
  // Record an impending self-write so the matching owner-changed signal
  // can be skipped. Accepts either a string (text path) or
  // { mime, bytes } (binary path).
  expectOwnWrite(arg) {
    const expiresUs = GLib.get_monotonic_time() + ClipboardIntake.SELF_WRITE_TTL_US;
    if (typeof arg === 'string') {
      if (arg.length === 0) {
        this._lastSelfWrite = null;
        return;
      }
      this._lastSelfWrite = { mime: PLAIN_TEXT_MIME, payload: arg, expiresUs };
      return;
    }
    if (arg && typeof arg === 'object' &&
        typeof arg.mime === 'string' && arg.mime.length > 0 &&
        arg.bytes instanceof Uint8Array && arg.bytes.length > 0) {
      this._lastSelfWrite = { mime: arg.mime, payload: arg.bytes, expiresUs };
      return;
    }
    this._lastSelfWrite = null;
  }
```

- [ ] **Step 6: Replace `_onSelectionChanged` with the enumerate-select-read flow**

Find the entire `_onSelectionChanged` method (from `_onSelectionChanged(selectionType) {` to its closing `}`) and replace it with:

```javascript
  _onSelectionChanged(selectionType) {
    if (!this._enabled) return;

    if (selectionType === Meta.SelectionType.SELECTION_CLIPBOARD) {
      // accepted
    } else if (selectionType === Meta.SelectionType.SELECTION_PRIMARY) {
      if (!this._processPrimary) return;
    } else {
      return; // SELECTION_DND etc.
    }

    if (this._settings.get_boolean('private-mode')) {
      return;
    }

    const stType =
      selectionType === Meta.SelectionType.SELECTION_PRIMARY
        ? St.ClipboardType.PRIMARY
        : St.ClipboardType.CLIPBOARD;

    const mimes = Clipboard.get_mimetypes(stType);
    const choice = selectBestMime(mimes);
    if (!choice) return;

    if (choice.isText) {
      this._readText(stType);
    } else {
      this._readBinary(stType, choice.mime);
    }
  }

  _readText(stType) {
    Clipboard.get_text(stType, (_clip, text) => {
      if (typeof text !== 'string' || text.length === 0) return;
      if (this._matchesSelfWrite(PLAIN_TEXT_MIME, text)) return;

      let payload = text;
      if (this._settings.get_boolean('strip-text')) {
        payload = payload.trim();
      }
      if (payload.length === 0) return;

      const bytes = TEXT_ENCODER.encode(payload);
      this._submit(bytes, PLAIN_TEXT_MIME);
    });
  }

  _readBinary(stType, mime) {
    Clipboard.get_content(stType, mime, (_clip, gbytes) => {
      const bytes = unwrapBytes(gbytes);
      if (!bytes || bytes.length === 0) return;
      if (this._matchesSelfWrite(mime, bytes)) return;
      this._submit(bytes, mime);
    });
  }

  // Returns true and consumes the marker if (mime, payload) matches the
  // last self-write within its TTL. Expired markers are also cleared.
  _matchesSelfWrite(mime, payload) {
    const sw = this._lastSelfWrite;
    if (!sw) return false;
    if (GLib.get_monotonic_time() > sw.expiresUs) {
      this._lastSelfWrite = null;
      return false;
    }
    if (sw.mime !== mime) return false;
    if (typeof sw.payload === 'string' && typeof payload === 'string') {
      if (sw.payload !== payload) return false;
    } else if (sw.payload instanceof Uint8Array && payload instanceof Uint8Array) {
      if (!bytesEqual(sw.payload, payload)) return false;
    } else {
      return false;
    }
    this._lastSelfWrite = null;
    return true;
  }

  _submit(bytes, mime) {
    this._client.add(bytes, mime).then(id => {
      const ok = id !== null;
      if (!ok) console.warn('ringboard: client.add returned null (server down?)');
      if (typeof this._onAddResult === 'function') this._onAddResult(ok);
    }).catch(e => {
      console.warn(`ringboard: client.add failed: ${e.message}`);
      if (typeof this._onAddResult === 'function') this._onAddResult(false);
    });
  }
}

// Coerce the GBytes returned by St.Clipboard.get_content into a
// Uint8Array. GJS exposes GBytes#toArray returning a Uint8Array view.
function unwrapBytes(gbytes) {
  if (!gbytes) return null;
  if (gbytes instanceof Uint8Array) return gbytes;
  if (typeof gbytes.toArray === 'function') return gbytes.toArray();
  return null;
}

function bytesEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}
```

Note: the new `}` after `_submit` closes the class. Make sure the existing trailing `}` for the class (the one that used to come after the old `_onSelectionChanged`) is removed — there should be exactly one closing `}` for the class, and the two helper functions live at module scope after it.

- [ ] **Step 7: Sanity-check that `mimePriority.js` still loads after the wiring change**

```bash
cd gnome-shell/extension && node --input-type=module -e "
  import('./lib/mimePriority.js').then(() => console.log('mimePriority loads ok'));
"
```

Expected: prints `mimePriority loads ok`. (We can't fully load `clipboardIntake.js` under Node because of `gi://` imports; this is only a sanity check that the pure module is intact.)

- [ ] **Step 8: Commit both files together**

```bash
git add gnome-shell/extension/lib/dbusClient.js gnome-shell/extension/lib/clipboardIntake.js
git commit -m "gnome-shell: intake captures any clipboard MIME via selectBestMime"
```

---

## Task 7: `menuController.selectAndPaste` marks binary self-writes

**Files:**
- Modify: `gnome-shell/extension/lib/menuController.js`

- [ ] **Step 1: Update the binary branch of `selectAndPaste`**

Find this block in `gnome-shell/extension/lib/menuController.js`:

```javascript
    if (isBinaryEntry(entry)) {
      const bytes = this._decodeBase64(entry.data);
      if (!bytes) {
        console.warn(`ringboard: failed to decode binary entry ${entry.id}`);
        return;
      }
      const mime = entry.mime_type || 'application/octet-stream';
      Clipboard.set_content(
        St.ClipboardType.CLIPBOARD,
        mime,
        new GLib.Bytes(bytes),
      );
    } else {
      if (this._intake) this._intake.expectOwnWrite(entry.data);
      Clipboard.set_text(St.ClipboardType.CLIPBOARD, entry.data);
    }
```

Replace it with:

```javascript
    if (isBinaryEntry(entry)) {
      const bytes = this._decodeBase64(entry.data);
      if (!bytes) {
        console.warn(`ringboard: failed to decode binary entry ${entry.id}`);
        return;
      }
      const mime = entry.mime_type || 'application/octet-stream';
      // Mark the impending write so intake skips the matching
      // owner-changed signal. Must happen before set_content because
      // the signal can be dispatched before this call returns.
      if (this._intake) this._intake.expectOwnWrite({ mime, bytes });
      Clipboard.set_content(
        St.ClipboardType.CLIPBOARD,
        mime,
        new GLib.Bytes(bytes),
      );
    } else {
      if (this._intake) this._intake.expectOwnWrite(entry.data);
      Clipboard.set_text(St.ClipboardType.CLIPBOARD, entry.data);
    }
```

- [ ] **Step 2: Commit**

```bash
git add gnome-shell/extension/lib/menuController.js
git commit -m "gnome-shell: mark binary self-writes so intake skips them"
```

---

## Task 8: End-to-end smoke test in a nested shell

This task is manual. It exists to catch GJS-specific runtime issues that the unit tests can't see (e.g. `Clipboard.get_mimetypes` returning a different type than expected, `St.ClipboardType` mismatches, `GBytes.toArray` not existing on this GNOME version).

**Files:**
- None (uses existing `/tmp/nested-with-ringboard.sh`).

- [ ] **Step 1: Rebuild and install the extension package**

The extension is installed at `~/.local/share/gnome-shell/extensions/ringboard@clipboard-history/`. Copy the modified files in place:

```bash
cp -v gnome-shell/extension/extension.js \
       gnome-shell/extension/confirmDialog.js \
       gnome-shell/extension/dataStructures.js \
       gnome-shell/extension/metadata.json \
       gnome-shell/extension/stylesheet.css \
       gnome-shell/extension/prefs.js \
       ~/.local/share/gnome-shell/extensions/ringboard@clipboard-history/

cp -v gnome-shell/extension/lib/*.js \
       ~/.local/share/gnome-shell/extensions/ringboard@clipboard-history/lib/
```

Expected: 6 top-level files + 4 lib files (clipboardIntake.js, dbusClient.js, menuController.js, mimePriority.js) copied. The `.test.mjs` file is not part of the installed extension.

- [ ] **Step 2: Launch the nested shell with its own ringboard-server**

```bash
dbus-run-session -- /tmp/nested-with-ringboard.sh
```

Expected: a nested GNOME Shell window opens. The extension is enabled (because dconf is shared with the host session — make sure it's enabled there first).

- [ ] **Step 3: Verify text intake still works**

Inside the nested shell, open `gnome-text-editor` (or any app — `WAYLAND_DISPLAY=wayland-1` + the nested `DBUS_SESSION_BUS_ADDRESS` is required if launching from outside). Copy some text. Click the panel button → the text entry should appear at the top of the history.

- [ ] **Step 4: Verify image intake**

Take a screenshot with `gnome-screenshot --area --clipboard` (or copy an image from a browser). Click the panel button → an image thumbnail should appear at the top of the history.

- [ ] **Step 5: Verify no duplicate on paste-back**

Click the image entry in the menu. The menu should close. Open the menu again — there should be **one** image entry, not two. (Self-write suppression check.)

- [ ] **Step 6: Password hint coverage**

`wl-copy` advertises only one MIME at a time, so we can't simulate a "password + plain-text" multi-target offer from the command line easily. Rely on the unit tests in Task 4 for this coverage. If you want to spot-check at runtime, copy from GNOME Keyring (which sets `x-kde-passwordManagerHint` alongside the password text) and confirm the entry does not appear in the menu.

- [ ] **Step 7: Close the nested shell, confirm host behavior unchanged**

Kill the nested shell. In the host, re-enable the extension (if you disabled it for the test). Copy text and an image. Confirm both appear in the host menu.

- [ ] **Step 8: Record acceptance and commit any tweaks**

If steps 3–5 pass cleanly and you needed no extra fixes, there's nothing to commit. If you patched something during the smoke test, commit it now with a descriptive message.

---

## Final cross-check

After all tasks are green:

- [ ] Run the full unit suite: `cd gnome-shell/extension && node --test lib/mimePriority.test.mjs` — expect 23 tests pass.
- [ ] Confirm no leftover `client.add(string)` callsites: `grep -rn "client\.add\|\.add(" gnome-shell/extension/lib/ | grep -v test`. Every `add` call should pass `(bytes, mime)`.
- [ ] Confirm the spec acceptance bullets are exercised: image thumbnail appears; password drop honored (best effort via wl-copy or unit-test only); paste-back doesn't duplicate; text path unchanged.
