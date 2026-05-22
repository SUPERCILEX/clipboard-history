import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import { selectBestMime } from './mimePriority.js';

const Clipboard = St.Clipboard.get_default();

const TEXT_ENCODER = new TextEncoder();
const PLAIN_TEXT_MIME = 'text/plain;charset=utf-8';

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
export class ClipboardIntake {
  constructor(client, settings) {
    this._client = client;
    this._settings = settings;
    this._enabled = false;
    this._selection = null;
    this._ownerChangedId = 0;
    this._settingsChangedId = 0;
    this._processPrimary = false;

    // Content-match suppression: when the extension writes to the clipboard
    // itself, it records the MIME, payload, and monotonic timestamp here.
    // The next owner-changed signal whose read matches this MIME + payload
    // (within TTL) is skipped. Counting signals is unreliable because
    // set_text / set_content on identical content doesn't always trigger
    // owner-changed.
    //
    // `payload` is a string for text writes, Uint8Array for binary writes.
    this._lastSelfWrite = null; // { mime, payload: string|Uint8Array, expiresUs }
  }

  // TTL (microseconds) for the self-write match window.
  static SELF_WRITE_TTL_US = 500_000;

  enable() {
    if (this._enabled) return;
    this._enabled = true;

    this._selection = Shell.Global.get().get_display().get_selection();
    this._ownerChangedId = this._selection.connect(
      'owner-changed',
      (_sel, type, _source) => this._onSelectionChanged(type),
    );
    console.debug('ringboard: intake enabled, listening for owner-changed');

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

  // Register a callback that fires after every server-add attempt with the
  // result (boolean ok). Used by the indicator to flip its icon between
  // connected and disconnected states when intake silently fails.
  setOnAddResult(cb) {
    this._onAddResult = cb;
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
