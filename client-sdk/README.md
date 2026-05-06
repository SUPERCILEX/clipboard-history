# Ringboard Client SDK

<a href="https://crates.io/crates/clipboard-history-client-sdk">![Crates.io Version](https://img.shields.io/crates/v/clipboard-history-client-sdk)</a>

This library provides high-level APIs for working with Ringboard.

- [`DatabaseReader`](src/ring_reader.rs): The entrypoint for reading Ringboard data.
- [`api`](src/api.rs): The client API for communicating with the Ringboard server to modify data.
- [`ui_actor`](src/ui_actor.rs): An actor style command processor to make writing interactive UIs
  built on top of Ringboard easy.
- [`search`](src/search.rs): A somewhat lower-level interface to execute high-performance queries
  against the Ringboard database. Used by the `ui_actor`.
- [`config`](src/config.rs): Internal shared config types between the CLI and other apps.
- [`duplicate_detection`](src/duplicate_detection.rs): A small helper for efficient duplicate entry
  detection.
- [`watcher_utils`](src/watcher_utils): Code shared between the X11 and Wayland watchers.
  - [`best_target`](src/watcher_utils/best_target.rs): Given the set of available mime types from
    which data can be copied from another application into Ringboard, determine the best one.
    Notably, this is where entries from well-behaved password managers get blocked so that Ringboard
    doesn't copy them.
  - [`deduplication`](src/watcher_utils/deduplication.rs): Similar to `duplicate_detection`, but for
    detecting would-be duplicates where the input is bytes to be copied rather than finding
    pre-existing duplicates within the databse.
