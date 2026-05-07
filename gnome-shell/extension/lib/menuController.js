import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Pango from 'gi://Pango';
import St from 'gi://St';

import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import { MAX_VISIBLE_CHARS, truncateLabel } from '../dataStructures.js';

const PAGE_SIZE = 50;
const SEARCH_DEBOUNCE_MS = 150;
const THUMB_SIZE = 64;

// Cache directory for image thumbnails. Files are written lazily on first
// render and reused across menu opens. Cleared on disable() to avoid
// indefinite growth.
const THUMB_CACHE_DIR = GLib.build_filenamev([
  GLib.get_user_cache_dir(),
  'ringboard-gnome-extension',
  'thumbs',
]);

// Map a MIME type like "image/png" to the file extension we want to use for
// the on-disk thumbnail copy. Falls back to "bin".
function mimeToExt(mime) {
  if (typeof mime !== 'string') return 'bin';
  const slash = mime.indexOf('/');
  if (slash < 0) return 'bin';
  const ext = mime.slice(slash + 1).split(';')[0].trim().toLowerCase();
  return ext || 'bin';
}

// True for entries that came from `ringboard debug dump` and carry a binary
// payload. Their `data` field is a base64-encoded string and `mime_type`
// describes the bytes.
function isBinaryEntry(entry) {
  return entry.kind === 'Bytes' && typeof entry.data === 'string';
}

// True specifically for binary entries whose payload is an image (PNG, WebP,
// etc.) so we can render a thumbnail instead of a generic placeholder.
function isImageEntry(entry) {
  return (
    isBinaryEntry(entry) &&
    typeof entry.mime_type === 'string' &&
    entry.mime_type.startsWith('image/')
  );
}

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

    try {
      GLib.mkdir_with_parents(THUMB_CACHE_DIR, 0o700);
    } catch (e) {
      console.warn(`ringboard: thumb cache mkdir failed: ${e.message}`);
    }
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

  // Best-effort cleanup of the thumbnail cache. Called from the extension's
  // disable() so we don't leave decoded image bytes on disk after teardown.
  dispose() {
    this._cancelDebounce();
    try {
      const dir = Gio.File.new_for_path(THUMB_CACHE_DIR);
      const enumerator = dir.enumerate_children(
        'standard::name',
        Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS,
        null,
      );
      let info;
      while ((info = enumerator.next_file(null)) !== null) {
        const child = dir.get_child(info.get_name());
        try { child.delete(null); } catch (_) {}
      }
      enumerator.close(null);
    } catch (_) {
      // Cache may not exist yet; ignore.
    }
  }

  // ---- lifecycle ----

  async onMenuOpen() {
    this._reset();
    this._fetchGen += 1;
    const myGen = this._fetchGen;
    // Empty query → dump (text + binary). Image entries only surface here.
    const entries = await this._client.dump();
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
    // Empty query goes through dump so image entries appear; non-empty uses
    // the text-only search command.
    const entries = query
      ? await this._client.search(query)
      : await this._client.dump();
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
    // Mark the impending write as our own BEFORE issuing it: the
    // owner-changed signal can be dispatched before this method returns,
    // and an unincremented counter would let intake re-add the entry.
    if (this._intake) this._intake.expectOwnWrite();
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
      Clipboard.set_text(St.ClipboardType.CLIPBOARD, entry.data);
    }

    if (this._settings.get_boolean('move-item-first')) {
      this._client.moveToFront(entry.id).catch(e => {
        console.warn(`ringboard: move-to-front failed: ${e.message}`);
      });
    }

    // Virtual Ctrl-V is text-oriented; for binary entries we set the
    // clipboard with the correct MIME type but don't simulate a paste
    // keystroke (apps that accept binary paste can use their own action).
    if (this._settings.get_boolean('paste-on-selection') && !isBinaryEntry(entry)) {
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
    // The menu may have been closed (which clears _resultEntries via _reset)
    // between an action firing and this render call; bail rather than touch a
    // historySection whose owning menu is being torn down.
    if (this._resultEntries === null) {
      return;
    }
    this._historySection.removeAll();

    const entries = this._resultEntries;
    const start = this._currentOffset;
    const slice = entries.slice(start, start + PAGE_SIZE);

    for (const entry of slice) {
      this._historySection.addMenuItem(this._buildItem(entry));
    }

    if (slice.length === 0) {
      const empty = new PopupMenu.PopupMenuItem(
        this._currentQuery ? 'No matches' : 'No clipboard history',
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

  _buildItem(entry) {
    let item;
    if (isImageEntry(entry)) {
      item = this._buildImageItem(entry);
    } else if (isBinaryEntry(entry)) {
      item = this._buildBinaryItem(entry);
    } else {
      item = this._buildTextItem(entry);
    }
    this._appendDeleteButton(item, entry);
    return item;
  }

  // Per-entry trash button on the right edge. Clicking it removes the entry
  // from the server and re-renders without dismissing the menu.
  _appendDeleteButton(item, entry) {
    const icon = new St.Icon({
      icon_name: 'edit-delete-symbolic',
      style_class: 'popup-menu-icon',
    });
    const btn = new St.Button({
      style_class: 'ci-action-btn',
      can_focus: true,
      child: icon,
      x_align: Clutter.ActorAlign.END,
      x_expand: false,
      y_expand: true,
    });
    btn.connect('clicked', () => {
      this.removeEntry(entry).catch(e => {
        console.warn(`ringboard: remove failed: ${e.message}`);
      });
    });
    item.actor.add_child(btn);
  }

  _buildTextItem(entry) {
    const item = new PopupMenu.PopupMenuItem('');
    const isText =
      entry.kind === 'Human' &&
      typeof entry.data === 'string' &&
      entry.data.length > 0;
    const labelText = isText
      ? truncateLabel(entry.data, MAX_VISIBLE_CHARS)
      : `[${entry.kind || 'unknown'}]`;
    item.label.set_text(labelText);
    const ct = item.label.get_clutter_text();
    ct.set_single_line_mode(true);
    ct.set_ellipsize(Pango.EllipsizeMode.END);
    if (!isText) {
      item.setSensitive(false);
    } else {
      item.connect('activate', () => {
        this.selectAndPaste(entry).catch(e => {
          console.warn(`ringboard: paste failed: ${e.message}`);
        });
      });
    }
    return item;
  }

  // Non-image binary entry (PDF, audio, archive, application/octet-stream,
  // etc.). We can't preview the bytes, so render a textual placeholder with
  // the MIME type and approximate size, and route activation through
  // selectAndPaste which will set_content with the correct MIME.
  _buildBinaryItem(entry) {
    const item = new PopupMenu.PopupMenuItem('');
    const mime = entry.mime_type || 'application/octet-stream';
    const sizeKB = Math.max(1, Math.round((entry.data.length * 3) / 4 / 1024));
    item.label.set_text(`[${mime} · ~${sizeKB} KB]`);
    const ct = item.label.get_clutter_text();
    ct.set_single_line_mode(true);
    ct.set_ellipsize(Pango.EllipsizeMode.END);
    item.connect('activate', () => {
      this.selectAndPaste(entry).catch(e => {
        console.warn(`ringboard: binary paste failed: ${e.message}`);
      });
    });
    return item;
  }

  _buildImageItem(entry) {
    const path = this._writeImageThumb(entry);
    const item = new PopupMenu.PopupMenuItem('');
    if (path) {
      const icon = new St.Icon({
        gicon: Gio.FileIcon.new(Gio.File.new_for_path(path)),
        icon_size: THUMB_SIZE,
        style_class: 'ci-image-thumb',
      });
      // PopupMenuItem already has a label as its first child; insert the icon
      // before it so the layout is [thumb][label].
      item.insert_child_at_index(icon, 0);
    }
    const sizeKB = Math.max(1, Math.round((entry.data.length * 3) / 4 / 1024));
    item.label.set_text(`${entry.mime_type} · ~${sizeKB} KB`);
    const ct = item.label.get_clutter_text();
    ct.set_single_line_mode(true);
    ct.set_ellipsize(Pango.EllipsizeMode.END);
    item.connect('activate', () => {
      this.selectAndPaste(entry).catch(e => {
        console.warn(`ringboard: image paste failed: ${e.message}`);
      });
    });
    return item;
  }

  // Decode an entry's base64 payload to a Uint8Array. Returns null on failure.
  _decodeBase64(b64) {
    try {
      return GLib.base64_decode(b64);
    } catch (e) {
      return null;
    }
  }

  // Write `entry.data` (base64) to the thumb cache under {id}.{ext}. Returns
  // the absolute path or null on failure. Existing files are reused.
  _writeImageThumb(entry) {
    try {
      const ext = mimeToExt(entry.mime_type);
      const path = GLib.build_filenamev([THUMB_CACHE_DIR, `${entry.id}.${ext}`]);
      if (GLib.file_test(path, GLib.FileTest.EXISTS)) {
        return path;
      }
      const bytes = this._decodeBase64(entry.data);
      if (!bytes) return null;
      const file = Gio.File.new_for_path(path);
      const stream = file.replace(
        null,
        false,
        Gio.FileCreateFlags.REPLACE_DESTINATION,
        null,
      );
      stream.write_bytes(new GLib.Bytes(bytes), null);
      stream.close(null);
      return path;
    } catch (e) {
      console.warn(`ringboard: thumb write failed for ${entry.id}: ${e.message}`);
      return null;
    }
  }
}
