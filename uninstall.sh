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

# The GNOME Wayland paste injector is a user shell extension. gnome-extensions
# uninstall unloads it, but does nothing when the shell never loaded it, so remove
# the files and the enabled-extensions entry directly.
gnome-extensions uninstall ringboard-paste@alexsaveau.dev 2> /dev/null || true
rm -rf ~/.local/share/gnome-shell/extensions/ringboard-paste@alexsaveau.dev
enabled=$(gsettings get org.gnome.shell enabled-extensions 2> /dev/null || true)
case "$enabled" in
  *ringboard-paste@alexsaveau.dev*)
    gsettings set org.gnome.shell enabled-extensions \
      "$(printf '%s' "$enabled" | sed -e "s/, \?'ringboard-paste@alexsaveau.dev'//" -e "s/'ringboard-paste@alexsaveau.dev',\? \?//")"
    ;;
esac

cargo uninstall \
  clipboard-history-server \
  clipboard-history-x11 \
  clipboard-history-wayland \
  clipboard-history-tui \
  clipboard-history-egui \
  clipboard-history
