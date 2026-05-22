# Extension binary clipboard intake — design

## Problem

Images copied to the clipboard never appear in the ringboard menu. The extension's intake reads only text, so non-text payloads (images, custom formats, URI lists) are dropped at the source. The menu's image-rendering path already exists and works — it just never receives any image entries.

Two reasons we can't rely on the existing `ringboard-wayland` watcher to fill this gap on GNOME Shell:

1. GNOME Shell is the Wayland compositor. The Wayland data-control protocol (which `ringboard-wayland` uses) is only exposed to non-compositor clients, so the watcher can't read the clipboard inside GNOME.
2. Even where it could run, having two intakes (watcher + extension) racing on the same selection produces duplicate entries.

The extension must become a complete intake on its own.

## What ringboard does upstream

Every copy event puts a *list of MIME types* on the clipboard. The source app advertises every format it can produce (`image/png`, `image/bmp`, `text/html`, `text/plain`, …); the destination picks one at paste time. Ringboard's wayland watcher already navigates this:

1. Compositor announces N MIME offers for the new selection.
2. Watcher accumulates them into `BestMimeTypeFinder` (`client-sdk/src/watcher_utils/best_target.rs`), which sorts MIMEs into six priority slots:
   1. plain text (`is_plaintext_mime`)
   2. `image/*`
   3. `x-special/*`
   4. `chromium/x-web-custom-data`
   5. any other `text/*`
   6. anything else starting with a lowercase letter
3. When the offer finalizes, `pop_best()` picks exactly one MIME from the highest-priority non-empty slot.
4. Watcher pipes bytes for that one MIME, sends one `AddRequest` to ringboard-server.

So ringboard stores **one entry per copy event**, with the canonical raw form. It does not duplicate one copy into multiple entries. Privacy filters live in the same module:

- `x-kde-passwordManagerHint` present anywhere in the offer → drop the entire entry.
- `chromium/x-internal*` MIMEs are ignored.
- MIMEs starting with an uppercase letter are ignored (app-internal targets like X11 atoms).
- Within a slot, prefer the MIME without `;` parameters over one with them.

## Design

Port that exact policy into the extension's intake. The compositor side (`St.Clipboard.get_mimetypes` / `get_content`) gives us the same information the wayland data-control protocol gives the watcher — just through a different API.

### Architecture

`clipboardIntake.js` runs a two-step read on every `owner-changed` event:

1. **Enumerate**: `Clipboard.get_mimetypes(stType) → string[]`.
2. **Select**: run a JS port of `BestMimeTypeFinder` → either a chosen MIME (with `isText` flag) or `null` (drop event).
3. **Read**:
   - text MIME → `Clipboard.get_text(stType, cb)` (keeps the existing `strip-text` setting and existing byte-encoding path).
   - binary MIME → `Clipboard.get_content(stType, mime, cb)` → `GBytes` → `Uint8Array`.
4. **Submit**: one `DbusClient.add(bytes, mime)` call per event.

Menu rendering needs no changes: `_buildImageItem` already handles `image/*` entries, `_buildBinaryItem` already shows a `[mime · ~size KB]` placeholder for anything else.

### Files

- **`lib/mimePriority.js`** (new). Single export `selectBestMime(mimes: string[]): { mime: string, isText: boolean } | null`. Pure function — no GJS imports, easy to unit-test in isolation. Mirrors `BestMimeTypeFinder` semantics: 6 priority slots, password/internal/uppercase filters, prefer-no-params within a slot.

- **`lib/clipboardIntake.js`** (modify). `_onSelectionChanged` calls `get_mimetypes`, then `selectBestMime`. Branches into text path (existing `get_text` flow) or binary path (new `get_content` flow). `_lastSelfWrite` carries `{ mime, payload, expiresUs }` where `payload` is a `string` (text path) or `Uint8Array` (binary path); comparison is string-equal or `Uint8Array` byte-equal.

- **`lib/dbusClient.js`** (modify). Generalise `add(text)` → `add(payloadBytes: Uint8Array, mime: string)`. The DBus method `Add(ay, s) → t` already accepts this shape; only the JS-side signature changes. Callers that used to pass a string now encode at the callsite. There are no external callers of `add`; this is an internal-only API.

- **`lib/menuController.js`** (small modify). In `selectAndPaste`, the binary branch now calls `expectOwnWrite({ mime, bytes })` before `Clipboard.set_content(...)`. The text branch is unchanged.

No server-side change. No new DBus method. The Add interface already takes `(ay, s)` — we've been under-using it.

### Priority algorithm

`selectBestMime(mimes)` runs through the offered MIMEs once and produces a single decision:

```
For each mime in mimes:
  if mime == "x-kde-passwordManagerHint":         password = true
  else if mime.startsWith("chromium/x-internal"): skip
  else if mime starts with uppercase:             skip
  else:
    slot = which-slot(mime)
    if slots[slot] is empty:                       slots[slot] = mime
    else if slots[slot] has ";" and mime doesn't:  slots[slot] = mime

If password: return null
Pick first non-empty slot in priority order.
Return { mime, isText: slot == PLAIN_TEXT }.
If no slot was filled: return null.
```

`which-slot` mirrors `is_plaintext_mime` and the slot mapping in `best_target.rs`:

- plain text slot: empty string, `text`, `text/plain`, `text/plain;charset=utf-8`, `TEXT`, `STRING`, `UTF8_STRING` (matches `ringboard-core`'s `is_plaintext_mime`).
- image slot: `image/*`.
- x-special slot: `x-special/*`.
- chromium-custom slot: exactly `chromium/x-web-custom-data`.
- any-text slot: other `text/*`.
- other slot: anything else starting lowercase (the password / chromium-internal / uppercase rules already filtered above).

`isText` is true only for the plain-text slot. Other text-class MIMEs (e.g. `text/html`) go through the binary path with their MIME preserved, matching what the watcher would do.

### Self-write suppression

`expectOwnWrite` already prevents the menu's paste action from re-ingesting its own clipboard write. We extend it to cover binary writes:

```
_lastSelfWrite = { mime: string, payload: string | Uint8Array, expiresUs: number }
```

`expectOwnWrite(arg)` accepts either a `string` (text path, current behavior) or `{ mime, bytes }` (binary path). The read callback compares `mime` first, then payload (string-equal or byte-equal). TTL stays at 500 ms.

Byte equality (not a hash) is fine for clipboard-sized payloads: a few MB worst case, one comparison per `owner-changed`. A counter-based suppression was tried earlier for text and failed — `set_content` on identical content doesn't always re-emit `owner-changed`, so counters drift. Content match works.

### Privacy & filters

The watcher's filters port directly:

- `x-kde-passwordManagerHint`: if present in the offer list, the whole intake drops. Critical: GNOME Keyring, 1Password, KWallet, and similar set this hint on password copies. Without it, every password copy gets logged into the ring.
- `chromium/x-internal*`: ignored.
- Uppercase-leading MIMEs: ignored (X11 atom names, app-internal targets).
- `private-mode` GSetting: unchanged, applies to both paths.
- `strip-text` GSetting: applies only when the chosen MIME is the plain-text slot. Binary payloads are sent verbatim.

### Edge cases

- Empty `get_mimetypes` result → drop event (nothing to read).
- `get_content` callback returns null bytes → drop event with a `console.warn`.
- Zero-length payload → drop (matches existing text rule).
- MIMEs offered but `selectBestMime` returns null (only filtered/password targets) → drop.
- Large payloads: no extension-side cap. The server already routes large entries to direct-file storage; DBus default message limit (~128 MB) is well above realistic clipboard sizes.

## Out of scope

- Rendering non-image binaries (PDFs, URI lists, custom formats) as anything richer than the existing `[mime · ~size KB]` placeholder. They will be captured by intake and shown as placeholders. Richer rendering is a follow-up.
- Virtual Ctrl-V for binary entries. Existing behavior (set_content only, no synthetic keypress) is preserved.
- Re-enabling `ringboard-wayland`. Not needed and would race with extension intake.
- Server-side changes. None required.

## Acceptance

- Copy an image (e.g. a screenshot) in any GNOME app. Open the menu. The image appears as a thumbnail row at the top, alongside any text entries copied later.
- Copy a password via GNOME Keyring (or any source setting `x-kde-passwordManagerHint`). The entry does **not** appear in the ring.
- Click an image entry in the menu. The image is restored to the clipboard with its original MIME (verifiable by pasting into an image-aware app). The same image is not duplicated as a new entry.
- Existing text intake, search, paste, paginate, delete, wipe behavior is unchanged.
