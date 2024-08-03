# Ringboard X11

<a href="https://crates.io/crates/clipboard-history-x11">![Crates.io Version](https://img.shields.io/crates/v/clipboard-history-x11)</a>

This binary provides an X11 clipboard watching service for the Ringboard database. It connects to
the X11 and Ringboard servers and uses the X11 XFixes extension to monitor the clipboard for new
clipboard selections to send to the Ringboard server.

Additionally, it offers a paste server capable of becoming the X11 selection owner for clients to
call.

## Implementation notes

- Primary selections are not supported.
- Blank and empty clipboard contents selections are not supported.
- A plain text fast path is implemented wherein an attempt will first be made to retrieve
  `UTF8_STRING` data before falling back to a `TARGETS` query.
- Target prioritization is implemented in [`best_target.rs`](src/best_target.rs).
- Best effort duplicate entry avoidance is provided with content hashing up to 4096 bytes and length
  hashing thereafter.
