#!/usr/bin/env bash
set -e

cargo +nightly install clipboard-history-server --no-default-features --features systemd
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/server/ringboard-server.service --create-dirs -O --output-dir ~/.config/systemd/user/

cargo +nightly install clipboard-history
cargo +nightly install clipboard-history-egui --no-default-features --features $XDG_SESSION_TYPE

# TODO remove once wayland client is implemented
XDG_SESSION_TYPE=x11
cargo +nightly install clipboard-history-$XDG_SESSION_TYPE --no-default-features
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/$XDG_SESSION_TYPE/ringboard-$XDG_SESSION_TYPE.service -O --output-dir ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable ringboard-$XDG_SESSION_TYPE --now

echo
echo "--- DONE ---"
echo
echo "Consider reading the egui docs:"
echo "https://github.com/SUPERCILEX/clipboard-history/blob/master/egui/README.md"
