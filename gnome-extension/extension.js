// SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later
//
// Ringboard paste injector for GNOME Shell. The injection technique is adapted,
// with attribution, from these MIT-licensed extensions:
//   - https://github.com/SUPERCILEX/gnome-clipboard-history
//   - https://github.com/Tudmotu/gnome-shell-extension-clipboard-indicator

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

// Keep these in sync with x11/src/main.rs.
const BUS_NAME = 'dev.alexsaveau.ringboard.Paste';
const OBJECT_PATH = '/dev/alexsaveau/ringboard/Paste';
const INTERFACE_NAME = 'dev.alexsaveau.ringboard.Paste';

const INTERFACE_XML = `
<node>
  <interface name="${INTERFACE_NAME}">
    <method name="Paste"/>
  </interface>
</node>`;

// Ringboard's own windows, matched by app-id prefix (Wayland uses the app id as
// the window class) so a new or renamed client cannot silently paste into
// Ringboard itself. The client hides only *after* the paste command reaches us.
const RINGBOARD_APP_ID_PREFIX = 'ringboard';

// Polled, not signalled: the window may already be gone when the call arrives,
// and a short bounded wait beats tracking window lifecycles.
const FOCUS_POLL_INTERVAL_MS = 20;
const FOCUS_WAIT_TIMEOUT_MS = 500;

// A burst this deep means a misbehaving caller, not a user.
const MAX_PENDING_PASTES = 8;

export default class RingboardPasteExtension extends Extension {
  enable() {
    // Created here rather than on the first paste: a device built and used in
    // the same event loop turn resolves the keysyms against a keymap that isn't
    // ready yet, which loses the modifiers and lands on the wrong key (observed
    // as PageDown, `^[[6~`, reaching the target instead of a paste).
    this._device = Clutter.get_default_backend()
      .get_default_seat()
      .create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
    this._pasteTimeoutId = 0;
    this._pendingPastes = 0;

    this._dbus = Gio.DBusExportedObject.wrapJSObject(INTERFACE_XML, this);
    this._dbus.export(Gio.DBus.session, OBJECT_PATH);
    this._busNameId = Gio.bus_own_name(
      Gio.BusType.SESSION,
      BUS_NAME,
      Gio.BusNameOwnerFlags.NONE,
      null,
      null,
      null,
    );
  }

  disable() {
    if (this._pasteTimeoutId) {
      GLib.source_remove(this._pasteTimeoutId);
      this._pasteTimeoutId = 0;
    }
    this._pendingPastes = 0;
    if (this._busNameId) {
      Gio.bus_unown_name(this._busNameId);
      this._busNameId = 0;
    }
    this._dbus?.unexport();
    this._dbus = null;
    this._device?.run_dispose();
    this._device = null;
  }

  /**
   * D-Bus entry point called by the Ringboard watcher. Returns immediately: the
   * keystroke is injected from a later event loop turn, so the caller is never
   * blocked by the focus wait.
   */
  Paste() {
    // The watcher sends one call per paste, so queue rather than drop: a dropped
    // request is a paste the user loses.
    if (this._pendingPastes >= MAX_PENDING_PASTES) {
      console.warn(
        `Dropping paste request: ${MAX_PENDING_PASTES} already queued.`,
      );
      return;
    }
    this._pendingPastes++;
    if (this._pasteTimeoutId) {
      return;
    }

    const deadline = GLib.get_monotonic_time() + FOCUS_WAIT_TIMEOUT_MS * 1000;
    this._pasteTimeoutId = GLib.timeout_add(
      GLib.PRIORITY_DEFAULT,
      FOCUS_POLL_INTERVAL_MS,
      () => {
        if (this._isRingboardFocused() && GLib.get_monotonic_time() < deadline) {
          return true;
        }
        this._pasteTimeoutId = 0;
        const pending = this._pendingPastes;
        this._pendingPastes = 0;
        for (let i = 0; i < pending; i++) {
          this._injectPaste();
        }
        return false;
      },
    );
  }

  _isRingboardFocused() {
    const appId = Shell.Global.get().display.focusWindow?.get_wm_class();
    return (
      typeof appId === 'string' &&
      appId.toLowerCase().startsWith(RINGBOARD_APP_ID_PREFIX)
    );
  }

  _injectPaste() {
    // Terminals bind Ctrl+Shift+Insert rather than Shift+Insert.
    const terminal =
      Main.inputMethod?.content_purpose === Clutter.InputContentPurpose.TERMINAL;
    const keys = terminal
      ? [Clutter.KEY_Control_L, Clutter.KEY_Shift_L, Clutter.KEY_Insert]
      : [Clutter.KEY_Shift_L, Clutter.KEY_Insert];

    for (const key of keys) {
      this._notifyKey(key, Clutter.KeyState.PRESSED);
    }
    for (const key of keys.slice().reverse()) {
      this._notifyKey(key, Clutter.KeyState.RELEASED);
    }
  }

  _notifyKey(keyval, state) {
    // notify_keyval wants microseconds; get_current_event_time() is milliseconds.
    this._device.notify_keyval(
      Clutter.get_current_event_time() * 1000,
      keyval,
      state,
    );
  }
}
