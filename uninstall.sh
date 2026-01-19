#!/usr/bin/env bash

systemctl --user disable ringboard-server --now
systemctl --user disable ringboard-x11 --now
systemctl --user disable ringboard-wayland --now
systemctl --user disable ringboard.slice --now
systemctl --user daemon-reload

rm ~/.config/systemd/user/ringboard*
rm ~/.local/share/applications/ringboard*
rm ~/.local/share/icons/hicolor/1024x1024/ringboard*
rm -r ~/.local/share/clipboard-history/

cargo uninstall \
  clipboard-history-server \
  clipboard-history-x11 \
  clipboard-history-wayland \
  clipboard-history-tui \
  clipboard-history-egui \
  clipboard-history
