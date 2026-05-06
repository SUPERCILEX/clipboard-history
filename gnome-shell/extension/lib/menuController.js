import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Pango from 'gi://Pango';
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
      const item = new PopupMenu.PopupMenuItem('');
      // Image and other non-text kinds get a placeholder; text data is
      // truncated for display.
      const isText = typeof entry.data === 'string' && entry.data.length > 0;
      const labelText = isText
        ? truncateLabel(entry.data, MAX_VISIBLE_CHARS)
        : `[${entry.kind || 'binary'}]`;
      item.label.set_text(labelText);
      // Force single-line display: ellipsize at the end if the menu width
      // can't fit the (already whitespace-collapsed, char-truncated) text.
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
