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
