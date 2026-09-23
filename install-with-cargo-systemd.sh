#!/usr/bin/env bash
set -e

# Downloads $1 to the curl args in $2.., failing loudly (instead of silently
# writing an empty/error-page file) if the request doesn't succeed.
fetch() {
  local url="$1"
  shift
  if ! curl -sSfL "$url" "$@"; then
    echo "Failed to download $url" >&2
    exit 1
  fi
}

fetch https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/ringboard.slice --create-dirs -O --output-dir ~/.config/systemd/user/

cargo install clipboard-history-server --no-default-features --features systemd
fetch https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/server/ringboard-server.service --create-dirs -O --output-dir ~/.config/systemd/user/
sed -i "s|ExecStart=ringboard-server|ExecStart=$(which ringboard-server)|g" ~/.config/systemd/user/ringboard-server.service

cargo install clipboard-history

cargo install clipboard-history-egui --no-default-features --features $XDG_SESSION_TYPE,avif || cargo install clipboard-history-egui --no-default-features --features $XDG_SESSION_TYPE
fetch https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/egui/ringboard-egui.desktop --create-dirs -O --output-dir ~/.local/share/applications/
fetch https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/logo.jpeg -o ringboard.jpeg --create-dirs -O --output-dir ~/.local/share/icons/hicolor/1024x1024/
sed -i "s|Exec=ringboard-egui|Exec=$(echo $(which ringboard-egui) toggle)|g" ~/.local/share/applications/ringboard-egui.desktop
sed -i "s|Icon=ringboard|Icon=$HOME/.local/share/icons/hicolor/1024x1024/ringboard.jpeg|g" ~/.local/share/applications/ringboard-egui.desktop

# Stop existing watchers in case user is switching between X11 and Wayland
systemctl --user disable ringboard-x11 --now 2> /dev/null || true
systemctl --user disable ringboard-wayland --now 2> /dev/null || true

if [ "$XDG_SESSION_TYPE" = "wayland" ]; then
  cargo install wayland-interface-check
  if [ "$XDG_CURRENT_DESKTOP" != "COSMIC" ] && ! wayland-interface-check ext_data_control_manager_v1; then
    export XDG_SESSION_TYPE=x11
  fi
fi

cargo install clipboard-history-$XDG_SESSION_TYPE --no-default-features
fetch https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/$XDG_SESSION_TYPE/ringboard-$XDG_SESSION_TYPE.service --create-dirs -O --output-dir ~/.config/systemd/user/
sed -i "s|ExecStart=ringboard-$XDG_SESSION_TYPE|ExecStart=$(which ringboard-$XDG_SESSION_TYPE)|g" ~/.config/systemd/user/ringboard-$XDG_SESSION_TYPE.service

killall ringboard-egui ringboard-tui 2> /dev/null || true

# The service won't exist yet on a first install.
systemctl --user stop ringboard-server 2> /dev/null || true
systemctl --user daemon-reload
systemctl --user start ringboard-server
systemctl --user enable ringboard-$XDG_SESSION_TYPE --now

# GNOME on Wayland uses the X11 watcher because Mutter has no data-control protocol,
# but XTEST there goes through the RemoteDesktop portal (prompting on every paste)
# and cannot reach native Wayland apps. The companion shell extension injects the
# keystroke from inside gnome-shell instead.
# Lowercased to match detect_injector(), which is case-insensitive: a watcher
# that selects the extension while the installer skipped it fails every paste.
desktops=$(echo "$XDG_CURRENT_DESKTOP" | tr '[:upper:]' '[:lower:]')
case ":$desktops:" in
  *:gnome:*) is_gnome=1 ;;
  *) is_gnome=0 ;;
esac
if [ "$is_gnome" = 1 ] && [ -n "$WAYLAND_DISPLAY" ]; then
  echo "Installing the Ringboard GNOME Shell extension..."
  ext_dir=$(mktemp -d)
  fetch https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/gnome-extension/metadata.json -O --output-dir "$ext_dir"
  fetch https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/gnome-extension/extension.js -O --output-dir "$ext_dir"
  (cd "$ext_dir" && zip -q ringboard-paste.zip metadata.json extension.js)
  gnome-extensions install --force "$ext_dir/ringboard-paste.zip"
  rm -rf "$ext_dir"

  # gnome-shell only discovers extensions at startup, so `gnome-extensions enable`
  # cannot work yet: pre-enable it through GSettings instead, which is what the
  # Extensions app does. Appending rather than overwriting keeps other extensions.
  current=$(gsettings get org.gnome.shell enabled-extensions)
  case "$current" in
    *ringboard-paste@alexsaveau.dev*) ;;
    *)
      if [ "$current" = "@as []" ]; then
        gsettings set org.gnome.shell enabled-extensions "['ringboard-paste@alexsaveau.dev']"
      else
        gsettings set org.gnome.shell enabled-extensions "${current%]}, 'ringboard-paste@alexsaveau.dev']"
      fi
      ;;
  esac

  echo
  echo "Log out and back in so GNOME Shell loads the extension. Pasting on GNOME"
  echo "Wayland will not work until it is loaded."
fi

echo
echo "--- DONE ---"
echo
echo "Consider reading the egui docs:"
echo "https://github.com/SUPERCILEX/clipboard-history/blob/master/egui/README.md"

if [ "$XDG_SESSION_TYPE" = "x11" ]; then
  echo
  echo "If you use a password manager and wish to exclude passwords from the clipboard, read the docs:"
  echo "https://github.com/SUPERCILEX/clipboard-history/blob/master/x11/README.md#password-manager-integration"
fi

if [ "$XDG_CURRENT_DESKTOP" = "COSMIC" ] && [ ! -f /etc/profile.d/clipboard.sh ]; then
  echo
  echo "COSMIC_DATA_CONTROL_ENABLED must be set, which requires sudo."
  echo "Please reboot after letting the following command run:"
  echo "$ sudo sh -c 'echo \"export COSMIC_DATA_CONTROL_ENABLED=1\" > /etc/profile.d/clipboard.sh; chmod 644 /etc/profile.d/clipboard.sh'"
  sudo sh -c 'echo "export COSMIC_DATA_CONTROL_ENABLED=1" > /etc/profile.d/clipboard.sh; chmod 644 /etc/profile.d/clipboard.sh'
fi
