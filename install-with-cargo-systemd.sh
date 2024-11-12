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
sed -i "s|Exec=ringboard-egui|Exec=$(echo /bin/sh -c \"ps -p \`cat /tmp/.ringboard/$USERNAME.egui-sleep 2\> /dev/null\` \> /dev/null 2\>\\\&1 \\\&\\\& exec rm -f /tmp/.ringboard/$USERNAME.egui-sleep \\\|\\\| exec $(which ringboard-egui)\")|g" ~/.local/share/applications/ringboard-egui.desktop
sed -i "s|Icon=ringboard|Icon=$HOME/.local/share/icons/hicolor/1024x1024/ringboard.jpeg|g" ~/.local/share/applications/ringboard-egui.desktop

# TODO remove once wayland client is implemented
XDG_SESSION_TYPE=x11
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
