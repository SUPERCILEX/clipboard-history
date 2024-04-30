#!/usr/bin/env bash
set -e

systemd --verison
cargo --version

cargo install clipboard-history-server --no-default-features --features systemd
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/server/ringboard-server.service --create-dirs -O --output-dir ~/.config/systemd/user/

cargo install clipboard-history-$XDG_SESSION_TYPE --no-default-features
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/$XDG_SESSION_TYPE/ringboard-$XDG_SESSION_TYPE.service -O --output-dir ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable ringboard-$XDG_SESSION_TYPE

cargo install clipboard-history clipboard-history-egui

echo
echo "--- DONE ---"
echo
echo "Consider adding a custom keyboard shortcut to start the GUI:"
echo "bash -c 'PATH=~/.cargo/bin:\$PATH ringboard-egui'"
