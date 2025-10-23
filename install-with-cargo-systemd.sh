#!/usr/bin/env bash
set -e

curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/ringboard.slice --create-dirs -O --output-dir ~/.config/systemd/user/

cargo +nightly install clipboard-history-server --no-default-features --features systemd
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/server/ringboard-server.service --create-dirs -O --output-dir ~/.config/systemd/user/
sed -i "s|ExecStart=ringboard-server|ExecStart=$(which ringboard-server)|g" ~/.config/systemd/user/ringboard-server.service

cargo +nightly install clipboard-history

cargo +nightly install clipboard-history-egui --no-default-features --features $XDG_SESSION_TYPE
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/egui/ringboard-egui.desktop --create-dirs -O --output-dir ~/.local/share/applications/
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/logo.jpeg -o ringboard.jpeg --create-dirs -O --output-dir ~/.local/share/icons/hicolor/1024x1024/
sed -i "s|Exec=ringboard-egui|Exec=$(echo $(which ringboard-egui) toggle)|g" ~/.local/share/applications/ringboard-egui.desktop
sed -i "s|Icon=ringboard|Icon=$HOME/.local/share/icons/hicolor/1024x1024/ringboard.jpeg|g" ~/.local/share/applications/ringboard-egui.desktop

# Stop existing watchers in case user is switching between X11 and Wayland
systemctl --user disable ringboard-x11 --now 2> /dev/null || true
systemctl --user disable ringboard-wayland --now 2> /dev/null || true

if [ "$XDG_SESSION_TYPE" = "wayland" ]; then
  cargo +nightly install wayland-interface-check
  if [ "$XDG_CURRENT_DESKTOP" != "COSMIC" ] && ! wayland-interface-check zwlr_data_control_manager_v1; then
    export XDG_SESSION_TYPE=x11
  fi
fi

cargo +nightly install clipboard-history-$XDG_SESSION_TYPE --no-default-features
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/$XDG_SESSION_TYPE/ringboard-$XDG_SESSION_TYPE.service -O --output-dir ~/.config/systemd/user/
sed -i "s|ExecStart=ringboard-$XDG_SESSION_TYPE|ExecStart=$(which ringboard-$XDG_SESSION_TYPE)|g" ~/.config/systemd/user/ringboard-$XDG_SESSION_TYPE.service

systemctl --user daemon-reload
systemctl --user enable ringboard-$XDG_SESSION_TYPE --now

killall ringboard-egui ringboard-tui 2> /dev/null || true
systemctl --user restart ringboard-server

echo
echo "--- DONE ---"
echo
echo "Consider reading the egui docs:"
echo "https://github.com/SUPERCILEX/clipboard-history/blob/master/egui/README.md"

if [ "$XDG_CURRENT_DESKTOP" = "COSMIC" ]; then
  echo "COSMIC_DATA_CONTROL_ENABLED must be set, which requires sudo."
  echo "Please reboot after letting the following command run:"
  echo "$ sudo sh -c 'echo \"export COSMIC_DATA_CONTROL_ENABLED=1\" > /etc/profile.d/clipboard.sh; chmod 644 /etc/profile.d/clipboard.sh'"
  sudo sh -c 'echo "export COSMIC_DATA_CONTROL_ENABLED=1" > /etc/profile.d/clipboard.sh; chmod 644 /etc/profile.d/clipboard.sh'
fi
