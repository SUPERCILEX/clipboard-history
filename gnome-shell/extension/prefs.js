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
