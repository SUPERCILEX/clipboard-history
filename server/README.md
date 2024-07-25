# Ringboard server

<a href="https://crates.io/crates/clipboard-history-server">![Crates.io Version](https://img.shields.io/crates/v/clipboard-history-server)</a>

This binary is the heart of Ringboard and is the only piece of code capable of writing data to the
database. The server waits for client commands over a Unix socket, processing and responding to
requests serially.

Important files:

- The [allocator](src/allocator.rs) is responsible for writing to the database.
- Requests are processed [here](src/requests.rs).
- The [reactor](src/reactor.rs) contains the io_uring event loop.
