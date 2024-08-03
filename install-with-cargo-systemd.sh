#!/usr/bin/env bash
set -e

cargo +nightly install clipboard-history-server --no-default-features --features systemd
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/server/ringboard-server.service --create-dirs -O --output-dir ~/.config/systemd/user/

# TODO remove once wayland client is implemented
XDG_SESSION_TYPE=x11
cargo +nightly install clipboard-history-$XDG_SESSION_TYPE --no-default-features
curl -s https://raw.githubusercontent.com/SUPERCILEX/clipboard-history/master/$XDG_SESSION_TYPE/ringboard-$XDG_SESSION_TYPE.service -O --output-dir ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable ringboard-$XDG_SESSION_TYPE --now

cargo +nightly install clipboard-history
cargo +nightly install clipboard-history-egui --no-default-features --features $XDG_SESSION_TYPE

echo
echo "--- DONE ---"
echo
echo "Consider adding a custom keyboard shortcut to start the GUI:"
echo "bash -c 'PATH=~/.cargo/bin:\$PATH ringboard-egui'"
