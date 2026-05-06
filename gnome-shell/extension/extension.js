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
import { openConfirmDialog } from './confirmDialog.js';

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
      openConfirmDialog(
        'Clear clipboard history',
        'This will remove every entry from the Ringboard server. Continue?',
        '',
        'Clear',
        'Cancel',
        () => resolve(true),
        () => resolve(false),
      );
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
    this._enableGen = (this._enableGen ?? 0) + 1;
    const myGen = this._enableGen;

    const binary = findBinary();
    if (!binary) {
      this._installIndicator(null, settings, false, myGen);
      console.warn('ringboard: CLI binary not found on $PATH');
      return;
    }
    const client = new SubprocessClient(binary);
    client.probe().then(connected => {
      this._installIndicator(client, settings, connected, myGen);
    }).catch(() => {
      this._installIndicator(client, settings, false, myGen);
    });
  }

  _installIndicator(client, settings, connected, gen) {
    // The extension may have been disabled (and possibly re-enabled) between
    // findBinary/probe and this callback; only mount if we still own this
    // enable generation.
    if (gen !== this._enableGen) return;
    if (this._indicator) return;
    this._indicator = new ClipboardIndicator(this, client, settings, connected);
    Main.panel.addToStatusArea('ringboard-clipboard-history', this._indicator, 1, 'right');
  }

  disable() {
    // Bump the generation so any pending probe callbacks become no-ops.
    this._enableGen = (this._enableGen ?? 0) + 1;
    if (this._indicator) {
      this._indicator.destroy();
      this._indicator = null;
    }
  }
}
