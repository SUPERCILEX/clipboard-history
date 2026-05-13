import Clutter from 'gi://Clutter';
import St from 'gi://St';
import GObject from 'gi://GObject';

import * as ModalDialog from 'resource:///org/gnome/shell/ui/modalDialog.js';

let _openDialog;

export function openConfirmDialog(
  title,
  message,
  sub_message,
  ok_label,
  cancel_label,
  callback,
  cancel_callback,
) {
  if (!_openDialog) {
    _openDialog = new ConfirmDialog(
      title,
      message + '\n' + sub_message,
      ok_label,
      cancel_label,
      callback,
      cancel_callback,
    ).open();
  } else if (typeof cancel_callback === 'function') {
    // A dialog is already showing; treat this duplicate request as cancelled
    // so the caller's Promise can settle instead of leaking forever.
    cancel_callback();
  }
}

const ConfirmDialog = GObject.registerClass(
  class ConfirmDialog extends ModalDialog.ModalDialog {
    _init(title, desc, ok_label, cancel_label, callback, cancel_callback) {
      super._init();

      let main_box = new St.BoxLayout({
        vertical: false,
      });
      this.contentLayout.add_child(main_box);

      let message_box = new St.BoxLayout({
        vertical: true,
      });
      main_box.add_child(message_box);

      let subject_label = new St.Label({
        style: 'font-weight: bold',
        x_align: Clutter.ActorAlign.CENTER,
        text: title,
      });
      message_box.add_child(subject_label);

      let desc_label = new St.Label({
        style: 'padding-top: 12px',
        x_align: Clutter.ActorAlign.CENTER,
        text: desc,
      });
      message_box.add_child(desc_label);

      this.setButtons([
        {
          label: cancel_label,
          action: () => {
            this.close();
            if (typeof cancel_callback === 'function') cancel_callback();
            _openDialog = null;
          },
          key: Clutter.Escape,
        },
        {
          label: ok_label,
          action: () => {
            this.close();
            callback();
            _openDialog = null;
          },
        },
      ]);
    }
  },
);
