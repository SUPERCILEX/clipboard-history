# Ringboard server

<a href="https://crates.io/crates/clipboard-history-server">![Crates.io Version](https://img.shields.io/crates/v/clipboard-history-server)</a>

This binary is the heart of Ringboard and is the only piece of code capable of writing data to the
database. The server waits for client commands over a Unix socket, processing and responding to
requests serially.

Important files:

- The [allocator](src/allocator.rs) is responsible for writing to the database.
- Requests are processed [here](src/requests.rs).
- The [reactor](src/reactor.rs) contains the io_uring event loop.

## DBus interface

When built with the default `dbus` feature, `ringboard-server` registers a
session-bus interface that mirrors the socket API for non-Rust callers.

- Bus name: `com.github.SUPERCILEX.Ringboard`
- Object path: `/com/github/SUPERCILEX/Ringboard`
- Interface: `com.github.SUPERCILEX.Ringboard1`

Methods:

| Method | Signature |
|---|---|
| `Add(ay payload, s mime)` | returns `t id` |
| `Search(s query, t offset, t limit)` | returns `(a(tsay) page, t total)` |
| `MoveToFront(t id)` | — |
| `Remove(t id)` | — |
| `Wipe()` | — |

`limit` is clamped to 500 rows per page. Empty `query` lists every entry
(text + binary, favorites then main). Build with
`--no-default-features --features systemd,human-logs` to disable.

Example:

```sh
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Add ays 5 104 101 108 108 111 "text/plain;charset=utf-8"
```
