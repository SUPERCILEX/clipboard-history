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

    // Content-match suppression: when the extension writes to the clipboard
    // itself, it records the payload and monotonic timestamp here. The next
    // owner-changed signal whose read matches this payload (within TTL) is
    // skipped. Counting signals is unreliable because set_text on identical
    // content doesn't always trigger owner-changed.
    this._lastSelfWrite = null; // { text: string, expiresUs: number }
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

    Clipboard.get_text(stType, (_clip, text) => {
      if (typeof text !== 'string' || text.length === 0) return;

      // Drop expired self-write marker, then skip if this read matches one
      // we just made ourselves.
      const sw = this._lastSelfWrite;
      if (sw) {
        if (GLib.get_monotonic_time() > sw.expiresUs) {
          this._lastSelfWrite = null;
        } else if (sw.text === text) {
          this._lastSelfWrite = null;
          return;
        }
      }

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
