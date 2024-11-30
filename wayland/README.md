# Ringboard Wayland

<a href="https://crates.io/crates/clipboard-history-wayland">![Crates.io Version](https://img.shields.io/crates/v/clipboard-history-wayland)</a>

This binary provides a Wayland clipboard watching service for the Ringboard database. It connects to
the Wayland and Ringboard servers and uses the `ext_data_control_v1` protocol to monitor the
clipboard for new clipboard selections to send to the Ringboard server.

Additionally, it offers a paste server capable of becoming the Wayland selection owner for clients
to call. Implementation notes are similar to the [X11 watcher](../x11).
