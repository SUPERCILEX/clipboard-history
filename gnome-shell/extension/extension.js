import Clutter from 'gi://Clutter';
import GObject from 'gi://GObject';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import {
  Extension,
  gettext as _,
} from 'resource:///org/gnome/shell/extensions/extension.js';

import { ensureActorVisibleInScrollView } from 'resource:///org/gnome/shell/misc/animationUtils.js';

import * as DS from './dataStructures.js';

// ringboard subprocess interface — initialised once per enable(), torn down on disable()
let _ffi = null;

/**
 * Locate the ringboard CLI binary.  Returns the path string or null.
 *
 * The CLI binary is named `ringboard` and is expected to be on PATH
 * (installed alongside the extension via cargo-install or a package).
 */
function _findCliBinary() {
  // Prefer an install-adjacent binary first.
  const candidates = [
    GLib.build_filenamev([GLib.get_home_dir(), '.cargo', 'bin', 'ringboard']),
    '/usr/local/bin/ringboard',
    '/usr/bin/ringboard',
  ];
  for (const p of candidates) {
    if (GLib.file_test(p, GLib.FileTest.IS_EXECUTABLE)) {
      return p;
    }
  }
  // Fall back to PATH search via `which`.
  try {
    const [ok, out] = GLib.spawn_command_line_sync('which ringboard');
    if (ok && out) {
      const path = new TextDecoder().decode(out).trim();
      if (path && GLib.file_test(path, GLib.FileTest.IS_EXECUTABLE)) {
        return path;
      }
    }
  } catch (_) {}
  return null;
}

/**
 * Build a subprocess-based ringboard client.
 *
 * GJS cannot call raw C function pointers returned by GModule.Module.symbol()
 * as JavaScript functions; the GModule approach is fundamentally non-functional
 * in the GJS runtime.  Instead we communicate with the ringboard server via
 * the `ringboard` CLI binary using Gio.Subprocess, which naturally
 * offloads all IPC and file I/O off the GNOME Shell main thread.
 *
 * Returns an object with async-friendly wrappers, or null if the CLI is not
 * found.
 */
function _loadFfi(_extensionPath) {
  const binary = _findCliBinary();
  if (!binary) {
    console.warn('ringboard: ringboard CLI binary not found — intake channel disabled');
    return null;
  }

  return {
    _binary: binary,

    /**
     * Add text to the ringboard server asynchronously.
     *
     * Uses Gio.Subprocess.communicate_utf8_async() so the subprocess stdin
     * write, stdout read, and process wait all happen off the GNOME Shell main
     * thread.  The callback is invoked on the main thread when the operation
     * completes (id >= 0 on success, id < 0 on error).
     */
    ringboard_add_text(text, callback) {
      let proc;
      try {
        proc = Gio.Subprocess.new(
          [binary, 'add', '-'],
          Gio.SubprocessFlags.STDIN_PIPE | Gio.SubprocessFlags.STDOUT_PIPE,
        );
      } catch (e) {
        console.warn('ringboard: add_text spawn error:', e.message);
        if (callback) {
          callback(-1);
        }
        return;
      }

      proc.communicate_utf8_async(text, null, (_proc, res) => {
        let id = -1;
        try {
          const [, stdout] = _proc.communicate_utf8_finish(res);
          if (stdout) {
            const line = stdout.trim();
            const parsed = parseInt(line, 10);
            if (!isNaN(parsed) && parsed >= 0) {
              id = parsed;
            }
          }
        } catch (e) {
          console.warn('ringboard: add_text async error:', e.message);
        }
        if (callback) {
          callback(id);
        }
      });
    },

    /**
     * Remove an entry from the ringboard server asynchronously.
     */
    ringboard_remove(ringboardId) {
      let proc;
      try {
        proc = Gio.Subprocess.new(
          [binary, 'remove', String(ringboardId)],
          Gio.SubprocessFlags.NONE,
        );
      } catch (e) {
        console.warn('ringboard: remove spawn error:', e.message);
        return;
      }
      proc.wait_async(null, (_proc, res) => {
        try {
          _proc.wait_finish(res);
        } catch (e) {
          console.warn('ringboard: remove async error:', e.message);
        }
      });
    },

    /**
     * Move an entry to the front of the ring asynchronously.
     */
    ringboard_move_to_front(ringboardId) {
      let proc;
      try {
        proc = Gio.Subprocess.new(
          [binary, 'move-to-front', String(ringboardId)],
          Gio.SubprocessFlags.NONE,
        );
      } catch (e) {
        console.warn('ringboard: move-to-front spawn error:', e.message);
        return;
      }
      proc.wait_async(null, (_proc, res) => {
        try {
          _proc.wait_finish(res);
        } catch (e) {
          console.warn('ringboard: move-to-front async error:', e.message);
        }
      });
    },

    ringboard_init() {
      // Connectivity check: verify the server is reachable by running a real
      // operation.  Using --help would always succeed even when the server is
      // down; `last` actually contacts the server socket.
      // Returns 0 on success, -1 on failure.
      try {
        const proc = Gio.Subprocess.new(
          [binary, 'last'],
          Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE,
        );
        proc.wait(null);
        return proc.get_successful() ? 0 : -1;
      } catch (e) {
        return -1;
      }
    },

    ringboard_destroy() {
      // No persistent resources to clean up in the subprocess approach.
    },
  };
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const Clipboard = St.Clipboard.get_default();
const VirtualKeyboard = (() => {
  let _vkb;
  return () => {
    if (!_vkb) {
      _vkb = Clutter.get_default_backend()
        .get_default_seat()
        .create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
    }
    return _vkb;
  };
})();

const SETTING_KEY_TOGGLE_MENU = 'toggle-menu';
const INDICATOR_ICON = 'edit-paste-symbolic';

const PAGE_SIZE = 50;
const MAX_VISIBLE_CHARS = 200;
const MAX_REGISTRY_LENGTH = 500;

// ---------------------------------------------------------------------------
// ClipboardIndicator panel button
// ---------------------------------------------------------------------------

class ClipboardIndicator extends PanelMenu.Button {
  _init(extension, ffi) {
    super._init(0, extension.indicatorName, false);

    this.extension = extension;
    this._ffi = ffi;
    this._shortcutsBindingIds = [];
    this._intakeEnabled = ffi !== null;

    const hbox = new St.BoxLayout({
      style_class: 'panel-status-menu-box clipboard-indicator-hbox',
    });
    this.icon = new St.Icon({
      icon_name: INDICATOR_ICON,
      style_class: 'system-status-icon clipboard-indicator-icon',
    });
    hbox.add_child(this.icon);
    this._downArrow = PopupMenu.arrowIcon(St.Side.BOTTOM);
    hbox.add_child(this._downArrow);
    this.add_child(hbox);

    this._buildMenu();
  }

  destroy() {
    this._unbindShortcuts();
    this._disconnectSelectionListener();

    if (this._searchFocusHackCallbackId) {
      GLib.Source.source_remove(this._searchFocusHackCallbackId);
      this._searchFocusHackCallbackId = undefined;
    }
    if (this._pasteHackCallbackId) {
      GLib.Source.source_remove(this._pasteHackCallbackId);
      this._pasteHackCallbackId = undefined;
    }

    super.destroy();
  }

  _buildMenu() {
    // Search entry
    this.searchEntry = new St.Entry({
      name: 'searchEntry',
      style_class: 'search-entry ci-history-search-entry',
      can_focus: true,
      hint_text: _('Search clipboard history…'),
      track_hover: true,
      x_expand: true,
      y_expand: true,
    });

    const entryItem = new PopupMenu.PopupBaseMenuItem({
      style_class: 'ci-history-search-section',
      reactive: false,
      can_focus: false,
    });
    entryItem.add_child(this.searchEntry);
    this.menu.addMenuItem(entryItem);

    this.menu.connect('open-state-changed', (self, open) => {
      if (open) {
        this._setMenuWidth();
        this.searchEntry.set_text('');
        this._searchFocusHackCallbackId = GLib.timeout_add(
          GLib.PRIORITY_DEFAULT,
          1,
          () => {
            global.stage.set_key_focus(this.searchEntry);
            this._searchFocusHackCallbackId = undefined;
            return false;
          },
        );
        this._refreshHistorySection();
      }
    });

    // History section
    this.historySection = new PopupMenu.PopupMenuSection();
    this.scrollViewMenuSection = new PopupMenu.PopupMenuSection();
    this.historyScrollView = new St.ScrollView({
      style_class: 'ci-history-menu-section',
      overlay_scrollbars: true,
    });
    this.historyScrollView.add_child(this.historySection.actor);
    this.scrollViewMenuSection.actor.add_child(this.historyScrollView);
    this.menu.addMenuItem(this.scrollViewMenuSection);

    this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

    // Actions row
    const actionsSection = new PopupMenu.PopupMenuSection();
    const actionsBox = new St.BoxLayout({
      style_class: 'ci-history-actions-section',
      vertical: false,
    });
    actionsSection.actor.add_child(actionsBox);
    this.menu.addMenuItem(actionsSection);

    const prevPage = new PopupMenu.PopupBaseMenuItem();
    prevPage.add_child(
      new St.Icon({ icon_name: 'go-previous-symbolic', style_class: 'popup-menu-icon' }),
    );
    prevPage.connect('activate', this._navigatePrevPage.bind(this));
    actionsBox.add_child(prevPage);

    const nextPage = new PopupMenu.PopupBaseMenuItem();
    nextPage.add_child(
      new St.Icon({ icon_name: 'go-next-symbolic', style_class: 'popup-menu-icon' }),
    );
    nextPage.connect('activate', this._navigateNextPage.bind(this));
    actionsBox.add_child(nextPage);

    actionsBox.add_child(new St.BoxLayout({ x_expand: true }));

    const clearMenuItem = new PopupMenu.PopupBaseMenuItem();
    clearMenuItem.add_child(
      new St.Icon({ icon_name: 'edit-delete-symbolic', style_class: 'popup-menu-icon' }),
    );
    clearMenuItem.connect('activate', this._clearHistory.bind(this));
    actionsBox.add_child(clearMenuItem);

    this.menu.actor.connect('key-press-event', (_, event) =>
      this._handleGlobalKeyEvent(event),
    );

    // In-memory entry store
    this.entries = new DS.LinkedList();
    this.activeHistoryMenuItems = 0;
    this.nextId = 0;

    this.currentlySelectedEntry = null;

    // Keyboard shortcut
    this._bindShortcuts();

    // Connect clipboard intake
    this._setupSelectionChangeListener();

    // Load existing history from ringboard server
    this._loadHistoryFromServer();

    // Search text change
    this.searchEntry
      .get_clutter_text()
      .connect('text-changed', this._onSearchTextChanged.bind(this));
  }

  _loadHistoryFromServer() {
    if (!this._ffi) return;

    try {
      const proc = Gio.Subprocess.new(
        [this._ffi._binary, 'search', '--json', ''],
        Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE,
      );
      proc.communicate_utf8_async(null, null, (_proc, res) => {
        try {
          const [, stdout] = _proc.communicate_utf8_finish(res);
          if (!stdout || !stdout.trim()) return;

          const entries = JSON.parse(stdout);
          // Entries come newest-first from search; we want oldest-first so
          // the most recent entry ends up at the front of the linked list.
          for (let i = entries.length - 1; i >= 0; i--) {
            const e = entries[i];
            if (!e.data || !e.data.trim()) continue;

            const node = new DS.LLNode();
            node.id = this.nextId++;
            node.type = DS.TYPE_TEXT;
            node.text = e.data;
            node.favorite = e.kind === 'Favorite';
            node.ringboardId = e.id;
            this.entries.append(node);
          }
          console.log(`ringboard: loaded ${this.entries.length()} entries from server`);
        } catch (e) {
          console.warn('ringboard: failed to parse history:', e.message);
        }
      });
    } catch (e) {
      console.warn('ringboard: failed to load history:', e.message);
    }
  }

  _setMenuWidth() {
    const display = global.display;
    const screenWidth = display.get_monitor_geometry(
      display.get_primary_monitor(),
    ).width;
    this.menu.actor.width = screenWidth * 0.35;
  }

  // -------------------------------------------------------------------------
  // Keyboard event handling
  // -------------------------------------------------------------------------

  _handleGlobalKeyEvent(event) {
    return (
      this._handleCtrlSelectKeyEvent(event) ||
      this._handleNavigationKeyEvent(event) ||
      this._handleFocusSearchKeyEvent(event) ||
      this._handleSearchTypeAheadKeyEvent(event)
    );
  }

  _handleCtrlSelectKeyEvent(event) {
    if (!event.has_control_modifier()) {
      return;
    }

    const index = parseInt(event.get_key_unicode()); // Starts at 1
    if (isNaN(index) || index <= 0) {
      return;
    }

    const items = this.historySection._getMenuItems();
    if (index > items.length) {
      return;
    }

    this._onMenuItemSelectedAndMenuClose(items[index - 1]);
    return true;
  }

  _handleNavigationKeyEvent(event) {
    if (!event.has_control_modifier()) {
      return;
    }

    if (event.get_key_unicode() === 'n') {
      this._navigateNextPage();
    } else if (event.get_key_unicode() === 'p') {
      this._navigatePrevPage();
    } else {
      return;
    }

    return true;
  }

  _handleFocusSearchKeyEvent(event) {
    if (event.get_key_unicode() !== '/') {
      return;
    }

    global.stage.set_key_focus(this.searchEntry);
    return true;
  }

  _handleSearchTypeAheadKeyEvent(event) {
    if (global.stage.get_key_focus() === this.searchEntry) {
      return false;
    }

    if (
      event.has_control_modifier() ||
      event.has_alt_modifier?.() ||
      event.has_super_modifier?.()
    ) {
      return false;
    }

    const keySymbol = event.get_key_symbol();
    if (keySymbol === Clutter.KEY_BackSpace) {
      const currentText = this.searchEntry.get_text();
      if (!currentText) {
        return false;
      }

      global.stage.set_key_focus(this.searchEntry);
      this.searchEntry.set_text(currentText.slice(0, -1));
      return true;
    }

    const unicode = event.get_key_unicode();
    if (!unicode || unicode.length !== 1) {
      return false;
    }

    const codePoint = unicode.charCodeAt(0);
    if (codePoint < 32 || codePoint === 127) {
      return false;
    }

    global.stage.set_key_focus(this.searchEntry);
    this.searchEntry.set_text(this.searchEntry.get_text() + unicode);
    return true;
  }

  // -------------------------------------------------------------------------
  // Menu items
  // -------------------------------------------------------------------------

  _addEntry(entry, selectEntry, insertIndex) {
    if (this.activeHistoryMenuItems >= PAGE_SIZE) {
      const items = this.historySection._getMenuItems();
      if (items.length > 0) {
        const item = items[items.length - 1];
        this._rewriteMenuItem(item, entry);
        this.historySection.moveMenuItem(item, 0);

        if (selectEntry) {
          this._selectEntry(entry);
        }
        return;
      }
    }

    const menuItem = new PopupMenu.PopupMenuItem('', { hover: false });
    menuItem.setOrnament(PopupMenu.Ornament.NONE);

    menuItem.entry = entry;
    entry.menuItem = menuItem;

    menuItem.connect('activate', this._onMenuItemSelectedAndMenuClose.bind(this));
    menuItem.connect('key-press-event', (_, event) =>
      this._handleMenuItemKeyEvent(event, menuItem),
    );

    this._setEntryLabel(menuItem);

    // Delete button
    const icon = new St.Icon({
      icon_name: 'edit-delete-symbolic',
      style_class: 'system-status-icon',
    });
    const icoBtn = new St.Button({
      style_class: 'ci-action-btn',
      can_focus: true,
      child: icon,
      x_align: Clutter.ActorAlign.END,
      x_expand: false,
      y_expand: true,
    });
    menuItem.actor.add_child(icoBtn);
    icoBtn.connect('clicked', () => {
      this._deleteEntryAndRestoreLatest(menuItem.entry);
    });

    menuItem.connect('destroy', () => {
      delete menuItem.entry.menuItem;
      this.activeHistoryMenuItems--;
    });
    menuItem.connect('key-focus-in', () => {
      ensureActorVisibleInScrollView(this.historyScrollView, menuItem);
    });

    this.historySection.addMenuItem(menuItem, insertIndex);
    this.activeHistoryMenuItems++;

    if (selectEntry) {
      this._selectEntry(entry);
    }
  }

  _handleMenuItemKeyEvent(event, menuItem) {
    if (event.get_key_code() === 119) {
      // 'w' key — delete
      const next = menuItem.entry.prev || menuItem.entry.next;
      if (next?.menuItem) {
        global.stage.set_key_focus(next.menuItem);
      }
      this._deleteEntryAndRestoreLatest(menuItem.entry);
    }
  }

  _setEntryLabel(menuItem) {
    const entry = menuItem.entry;
    if (entry.type === DS.TYPE_TEXT) {
      menuItem.label.set_text(this._truncated(entry.text, MAX_VISIBLE_CHARS));
    } else {
      // Non-text content (image, binary): show a placeholder preview label.
      menuItem.label.set_text(DS.placeholderLabel(entry.type, entry.mimeType));
    }
  }

  _rewriteMenuItem(item, entry) {
    if (item.entry.id === this.currentlySelectedEntry?.id) {
      item.setOrnament(PopupMenu.Ornament.NONE);
    }

    item.entry = entry;
    entry.menuItem = item;

    this._setEntryLabel(item);
    if (entry.id === this.currentlySelectedEntry?.id) {
      item.setOrnament(PopupMenu.Ornament.DOT);
    }
  }

  // -------------------------------------------------------------------------
  // Pagination
  // -------------------------------------------------------------------------

  _refreshHistorySection() {
    this.historySection.removeAll();
    this.activeHistoryMenuItems = 0;

    for (
      let entry = this.entries.last();
      entry && this.activeHistoryMenuItems < PAGE_SIZE;
      entry = entry.prev
    ) {
      this._addEntry(entry, this.currentlySelectedEntry === entry);
    }
  }

  _navigatePrevPage() {
    if (this.searchEntryFront) {
      this.populateSearchResults(this.searchEntry.get_text(), false);
      return;
    }

    const items = this.historySection._getMenuItems();
    if (items.length === 0) {
      return;
    }

    const start = items[0].entry;
    for (
      let entry = start.nextCyclic(), i = items.length - 1;
      entry !== start && i >= 0;
      entry = entry.nextCyclic()
    ) {
      this._rewriteMenuItem(items[i--], entry);
    }
  }

  _navigateNextPage() {
    if (this.searchEntryFront) {
      this.populateSearchResults(this.searchEntry.get_text(), true);
      return;
    }

    const items = this.historySection._getMenuItems();
    if (items.length === 0) {
      return;
    }

    const start = items[items.length - 1].entry;
    for (
      let entry = start.prevCyclic(), i = 0;
      entry !== start && i < items.length;
      entry = entry.prevCyclic()
    ) {
      this._rewriteMenuItem(items[i++], entry);
    }
  }

  // -------------------------------------------------------------------------
  // Search
  // -------------------------------------------------------------------------

  _onSearchTextChanged() {
    const query = this.searchEntry.get_text();

    if (!query) {
      this.searchEntryFront = this.searchEntryBack = undefined;
      this._refreshHistorySection();
      return;
    }

    this.searchEntryFront = this.searchEntryBack = this.entries.last();
    this.populateSearchResults(query);
  }

  populateSearchResults(query, forward) {
    if (!this.searchEntryFront) {
      return;
    }

    this.historySection.removeAll();
    this.activeHistoryMenuItems = 0;

    if (typeof forward !== 'boolean') {
      forward = true;
    }

    query = query.toLowerCase();
    let searchExp;
    try {
      searchExp = new RegExp(query, 'i');
    } catch {}
    const start = forward ? this.searchEntryFront : this.searchEntryBack;
    let entry = start;

    while (this.activeHistoryMenuItems < PAGE_SIZE) {
      if (entry.type === DS.TYPE_TEXT) {
        let match = entry.text.toLowerCase().indexOf(query);
        if (searchExp && match < 0) {
          match = entry.text.search(searchExp);
        }
        if (match >= 0) {
          this._addEntry(
            entry,
            entry === this.currentlySelectedEntry,
            forward ? undefined : 0,
          );
          if (entry.menuItem) {
            entry.menuItem.label.set_text(
              this._truncated(
                entry.text,
                match - 40,
                match + MAX_VISIBLE_CHARS - 40,
              ),
            );
          }
        }
      }

      entry = forward ? entry.prevCyclic() : entry.nextCyclic();
      if (entry === start) {
        break;
      }
    }

    if (forward) {
      this.searchEntryBack = this.searchEntryFront.nextCyclic();
      this.searchEntryFront = entry;
    } else {
      this.searchEntryFront = this.searchEntryBack.prevCyclic();
      this.searchEntryBack = entry;
    }
  }

  // -------------------------------------------------------------------------
  // Entry selection and clipboard interaction
  // -------------------------------------------------------------------------

  _selectEntry(entry, triggerPaste) {
    this.currentlySelectedEntry?.menuItem?.setOrnament(PopupMenu.Ornament.NONE);
    this.currentlySelectedEntry = entry;

    entry.menuItem?.setOrnament(PopupMenu.Ornament.DOT);

    if (entry.type === DS.TYPE_TEXT) {
      this._setClipboardText(entry.text);
    }

    if (triggerPaste) {
      this._triggerPasteHack();
    }
  }

  _setClipboardText(text) {
    if (this._debouncing !== undefined) {
      // Two clipboard types are set below (CLIPBOARD and PRIMARY), which fires
      // two owner-changed signals.  Increment by 2 so both are suppressed.
      this._debouncing += 2;
    }

    Clipboard.set_text(St.ClipboardType.CLIPBOARD, text);
    Clipboard.set_text(St.ClipboardType.PRIMARY, text);
  }

  _triggerPasteHack() {
    this._pasteHackCallbackId = GLib.timeout_add(
      GLib.PRIORITY_DEFAULT,
      1,
      () => {
        const SHIFT_L = 42;
        const INSERT = 110;

        const eventTime = Clutter.get_current_event_time() * 1000;
        VirtualKeyboard().notify_key(
          eventTime,
          SHIFT_L,
          Clutter.KeyState.PRESSED,
        );
        VirtualKeyboard().notify_key(
          eventTime,
          INSERT,
          Clutter.KeyState.PRESSED,
        );
        VirtualKeyboard().notify_key(
          eventTime,
          INSERT,
          Clutter.KeyState.RELEASED,
        );
        VirtualKeyboard().notify_key(
          eventTime,
          SHIFT_L,
          Clutter.KeyState.RELEASED,
        );

        this._pasteHackCallbackId = undefined;
        return false;
      },
    );
  }

  _onMenuItemSelectedAndMenuClose(menuItem) {
    this._selectEntry(menuItem.entry, true);
    this.menu.close();
  }

  // -------------------------------------------------------------------------
  // History management
  // -------------------------------------------------------------------------

  _clearHistory() {
    // Remove all entries from the durable ringboard server before wiping the
    // in-memory list, so users clearing sensitive clipboard data are fully
    // served — not just the UI cache.
    if (this._intakeEnabled) {
      for (
        let entry = this.entries.head;
        entry;
        entry = entry.next
      ) {
        if (entry.ringboardId !== undefined) {
          try {
            this._ffi.ringboard_remove(entry.ringboardId);
          } catch (e) {
            console.warn('ringboard: clear-history remove failed:', e.message);
          }
        }
      }
    }

    this.currentlySelectedEntry = null;
    this.entries = new DS.LinkedList();
    this.historySection.removeAll();
    this.activeHistoryMenuItems = 0;
    this._setClipboardText('');
  }

  _removeEntry(entry) {
    if (entry.id === this.currentlySelectedEntry?.id) {
      this.currentlySelectedEntry = null;
    }
    entry.menuItem?.destroy();
  }

  _deleteEntryAndRestoreLatest(entry) {
    const wasSelected = entry.id === this.currentlySelectedEntry?.id;

    // Notify ringboard server via FFI
    if (this._intakeEnabled && entry.ringboardId !== undefined) {
      try {
        this._ffi.ringboard_remove(entry.ringboardId);
      } catch (e) {
        console.warn('ringboard: remove failed:', e.message);
      }
    }

    entry.detach();
    this._removeEntry(entry);

    if (wasSelected) {
      const nextEntry = this.entries.last();
      if (nextEntry) {
        this._selectEntry(nextEntry, false);
      } else {
        this._setClipboardText('');
      }
    }
  }

  // -------------------------------------------------------------------------
  // Clipboard intake
  // -------------------------------------------------------------------------

  _shouldAbortClipboardQuery(kind) {
    return false;
  }

  _queryClipboard() {
    if (this._shouldAbortClipboardQuery(St.ClipboardType.CLIPBOARD)) {
      return;
    }

    Clipboard.get_text(St.ClipboardType.CLIPBOARD, (_, text) => {
      this._processClipboardContent(text, true, true);
    });
  }

  _queryPrimaryClipboard() {
    if (this._shouldAbortClipboardQuery(St.ClipboardType.PRIMARY)) {
      return;
    }

    // PRIMARY selection is text the user merely highlights, with no explicit
    // copy action.  Track it in the UI list but do NOT persist it to the
    // durable ringboard server without an explicit opt-in setting.
    Clipboard.get_text(St.ClipboardType.PRIMARY, (_, text) => {
      this._processClipboardContent(text, false, false);
    });
  }

  _processClipboardContent(text, selectEntry, persistToServer) {
    if (this._debouncing > 0) {
      this._debouncing--;
      return;
    }

    if (!text || !text.trim()) {
      return;
    }

    // De-duplicate: check if already in history
    let entry = this.entries.findTextItem(text);
    if (entry) {
      const isFirst = entry === this.entries.last();
      if (!isFirst) {
        // Move to front
        this.entries.append(entry);
        if (entry.menuItem) {
          this.historySection.moveMenuItem(entry.menuItem, 0);
        }
      }
      if (selectEntry) {
        this._selectEntry(entry, false);
      }
      return;
    }

    // New entry
    entry = new DS.LLNode();
    entry.id = this.nextId++;
    entry.type = DS.TYPE_TEXT;
    entry.text = text;
    entry.favorite = false;
    entry.ringboardId = undefined;
    this.entries.append(entry);

    // Submit to ringboard server asynchronously (off the Shell main thread).
    // PRIMARY selection is only persisted if the caller opts in via persistToServer.
    if (this._intakeEnabled && persistToServer) {
      try {
        this._ffi.ringboard_add_text(text, (id) => {
          if (id >= 0) {
            entry.ringboardId = id;
          } else {
            console.warn('ringboard: add_text returned error code:', id);
          }
        });
      } catch (e) {
        console.warn('ringboard: add_text failed:', e.message);
      }
    }

    this._addEntry(entry, selectEntry, 0);

    // Prune oldest if over limit
    while (this.entries.length > MAX_REGISTRY_LENGTH) {
      const oldest = this.entries.head;
      if (!oldest) break;
      oldest.detach();
      oldest.menuItem?.destroy();
    }
  }

  // -------------------------------------------------------------------------
  // Selection / clipboard change listener
  // -------------------------------------------------------------------------

  _setupSelectionChangeListener() {
    this._debouncing = 0;

    this.selection = Shell.Global.get().get_display().get_selection();
    this._selectionOwnerChangedId = this.selection.connect(
      'owner-changed',
      (_, selectionType) => {
        if (selectionType === Meta.SelectionType.SELECTION_CLIPBOARD) {
          this._queryClipboard();
        } else if (selectionType === Meta.SelectionType.SELECTION_PRIMARY) {
          this._queryPrimaryClipboard();
        }
      },
    );
  }

  _disconnectSelectionListener() {
    if (!this._selectionOwnerChangedId) {
      return;
    }

    this.selection.disconnect(this._selectionOwnerChangedId);
    this.selection = undefined;
    this._selectionOwnerChangedId = undefined;
  }

  // -------------------------------------------------------------------------
  // Keyboard shortcuts
  // -------------------------------------------------------------------------

  _bindShortcuts() {
    this._unbindShortcuts();
    this._bindShortcut(SETTING_KEY_TOGGLE_MENU, () => this.menu.toggle());
  }

  _unbindShortcuts() {
    this._shortcutsBindingIds.forEach((id) => Main.wm.removeKeybinding(id));
    this._shortcutsBindingIds = [];
  }

  _bindShortcut(name, cb) {
    const ModeType = Shell.hasOwnProperty('ActionMode')
      ? Shell.ActionMode
      : Shell.KeyBindingMode;

    try {
      Main.wm.addKeybinding(
        name,
        this.extension.getSettings?.() ?? new Map(),
        Meta.KeyBindingFlags.NONE,
        ModeType.ALL,
        cb.bind(this),
      );
      this._shortcutsBindingIds.push(name);
    } catch (e) {
      console.warn('ringboard: keybinding registration failed for', name, ':', e.message);
    }
  }

  // -------------------------------------------------------------------------
  // Helpers
  // -------------------------------------------------------------------------

  _truncated(s, start, end) {
    if (start < 0) {
      start = 0;
    }
    if (!end) {
      end = start;
      start = 0;
    }
    if (end > s.length) {
      end = s.length;
    }

    const includesStart = start === 0;
    const includesEnd = end === s.length;
    const isMiddle = !includesStart && !includesEnd;
    const length = end - start;
    const overflow = s.length > length;

    s = s.substring(start, end + 100);
    s = s.replace(/\s+/g, ' ').trim();

    if (includesStart && overflow) {
      s = s.substring(0, length - 1) + '…';
    }
    if (includesEnd && overflow) {
      s = '…' + s.substring(1, length);
    }
    if (isMiddle) {
      s = '…' + s.substring(1, length - 1) + '…';
    }

    return s;
  }
}

const ClipboardIndicatorObj = GObject.registerClass(ClipboardIndicator);

// ---------------------------------------------------------------------------
// Extension entry point
// ---------------------------------------------------------------------------

export default class RingboardExtension extends Extension {
  enable() {
    this.indicatorName = `${this.metadata.name} Indicator`;

    // Locate the ringboard CLI binary and build the subprocess-based client.
    // If the binary is not found the extension degrades to browse-only mode.
    try {
      _ffi = _loadFfi(this.path);
      if (!_ffi) {
        console.warn('ringboard: CLI binary not available — intake channel disabled, browse-only mode');
      }
    } catch (e) {
      console.warn('ringboard: init error:', e.message, '— intake disabled');
      _ffi = null;
    }

    this.clipboardIndicator = new ClipboardIndicatorObj(this, _ffi);
    Main.panel.addToStatusArea(this.indicatorName, this.clipboardIndicator, 1);
  }

  disable() {
    if (this.clipboardIndicator) {
      this.clipboardIndicator.destroy();
      this.clipboardIndicator = undefined;
    }

    // Clean up subprocess client (no persistent OS resources to release).
    if (_ffi) {
      try {
        _ffi.ringboard_destroy();
      } catch (e) {
        console.warn('ringboard: destroy error:', e.message);
      }
      _ffi = null;
    }
  }
}

// Top-level enable function (compatibility shim for legacy GNOME Shell tooling)
export function enable() {
  // The Extension class handles enable/disable for modern GNOME Shell 46+.
}

// Top-level disable function (compatibility shim for legacy GNOME Shell tooling)
export function disable() {
}
