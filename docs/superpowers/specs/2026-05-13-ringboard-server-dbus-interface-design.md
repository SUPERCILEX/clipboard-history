# Ringboard Server DBus Interface — Design

**Status:** Approved, ready for implementation plan
**Branch base:** `upstream/master` @ `86f94f9` (new branch: `feat/dbus-interface`)
**Crate touched:** `server/` only

## Goal

Expose the ringboard server's API on the session DBus so non-Rust clients
(notably the GNOME Shell extension at `gnome-shell/extension/`) can replace
per-call `ringboard` CLI subprocess invocations with native DBus calls. The
existing Unix-domain-socket protocol remains the canonical interface; DBus
is an additional front-end.

## Non-Goals

- Pushing `NewEntry` / change-notification signals over DBus. Not part of
  v1. Clients keep their own clipboard intake.
- Adding DBus to the client-sdk crate or to the watcher binaries.
- A system-bus interface. Session bus only — ringboard is a per-user daemon.
- Replacing or deprecating the socket protocol. CLI and Rust clients keep
  using it unchanged.
- Touching `gnome-shell/extension/` in this work. That migration happens in
  a separate spec, after this one ships.

## Architecture

The DBus front-end is hosted by `ringboard-server` itself, but is
implemented as an **embedded client**, not as new server logic:

```
                              ┌───────────────────────────┐
session bus (DBus) ◀──────────┤  dbus worker thread       │
                              │  tokio current-thread rt  │
                              │  zbus connection          │
                              │  ringboard-sdk client     │
                              └─────────────┬─────────────┘
                                            │ Unix socket
                                            │ (existing protocol,
                                            │  loopback to self)
                                            ▼
                              ┌───────────────────────────┐
                              │  reactor.rs (unchanged)   │
                              │  io_uring loop            │
                              └───────────────────────────┘
```

Properties this gives us:

- The DBus worker thread links the existing `ringboard-sdk` (already a
  workspace member, used by the CLI) and talks to its own server's Unix
  socket. Every DBus method maps 1:1 to an SDK call.
- `reactor.rs`, `requests.rs`, `allocator.rs`, `io_uring.rs`,
  `send_msg_bufs.rs`, `startup.rs` are **not modified**. No async runtime
  is introduced into the io_uring loop.
- The DBus thread holds a `tokio` current-thread runtime running zbus.
  zbus is the canonical pure-Rust DBus library and the only async dep
  added to the server crate.

## Module layout

A single new module:

- **`server/src/dbus.rs`** — public function `spawn(shutdown: Receiver<()>)`
  returning a `JoinHandle<()>`. Inside it:
  - sets up a tokio current-thread runtime,
  - opens a session-bus connection via `zbus::ConnectionBuilder`,
  - registers an `Interface` object that holds an `ringboard_sdk::Client`,
  - serves until the shutdown channel fires.

The interface methods are thin wrappers — each method takes the DBus
arguments, calls the matching SDK function, and either returns the SDK's
result or maps the SDK error to a `zbus::fdo::Error` variant.

`main.rs` gains a single call after `claim_server_ownership()`:

```rust
#[cfg(feature = "dbus")]
let dbus_handle = dbus::spawn(shutdown_rx);
```

and on shutdown, it sends on `shutdown_tx` and joins the handle.

## Feature gating

A new cargo feature `dbus`, included in the default feature set:

```toml
[features]
default = ["systemd", "human-logs", "dbus"]
dbus = ["dep:zbus", "dep:tokio"]
```

Disabling the feature compiles the server with zero DBus code and zero
extra dependencies — same binary as today.

## DBus surface

- **Bus name:** `com.github.SUPERCILEX.Ringboard`
- **Object path:** `/com/github/SUPERCILEX/Ringboard`
- **Interface:** `com.github.SUPERCILEX.Ringboard1`

| Method | Signature (in → out) | Maps to SDK |
|---|---|---|
| `Add` | `(ay payload, s mime) → t id` | `AddRequest` |
| `Search` | `(s query, t offset, t limit) → (a(tsay) page, t total)` | `SearchRequest` + slicing |
| `MoveToFront` | `(t id) → ()` | `MoveToFrontRequest` |
| `Remove` | `(t id) → ()` | `RemoveRequest` |
| `Wipe` | `() → ()` | `WipeRequest` |

Notes:
- `Add` takes raw bytes (`ay`) plus an explicit MIME so the GNOME extension
  can push images and arbitrary binary payloads without base64 round-trips
  (the CLI today only accepts text on stdin).
- `Search` is the single listing entry point:
  - **Empty query** lists **every** entry (text + binary) newest-first,
    replacing the current `ringboard debug dump` path used by the
    GNOME extension for unfiltered browsing.
  - **Non-empty query** runs a text search (matches today's
    `ringboard search` semantics; binary entries are not matched).
  - The return tuple is `(page, total)`. `page` is the slice
    `[offset, offset+limit)`; `total` is the full result-set size so
    clients can decide whether more pages exist. If `offset >= total`
    the page is empty.
  - Pagination drifts under concurrent inserts (rows shift by one when
    a new entry lands at the front). This is the same trade-off the
    current JS code accepts; durable cursor support is explicitly
    out-of-scope for v1.
  - `limit` is capped server-side at **500** rows per page; larger
    values are silently clamped. `limit = 0` returns an empty page and
    the live total (useful as a count probe).
- Tuples carry `(id, mime, payload)` for text and binary alike — clients
  branch on MIME. Text payloads are UTF-8 in the `ay` field; binary
  payloads are the raw bytes.
- IDs are `u64` (DBus `t`) to match the SDK's `EntryId`.

## Error handling

SDK errors are translated to `zbus::fdo::Error`:

- `Disconnected` / IO error → `Failed`, with the SDK error's display string.
- Validation (empty payload, etc.) → `InvalidArgs`.
- Unknown id on `Remove` / `MoveToFront` → `Failed` with a descriptive
  message. (No DBus-level "not found" exists; `Failed` is the closest fit.)

Failure to claim the bus name at startup (e.g., another process owns it,
or no session bus) is logged at `warn!` and the server continues — socket
clients are unaffected. The DBus thread exits, the worker is not retried.

## Lifecycle

- **Startup:** `main.rs` spawns the DBus worker thread after the reactor's
  socket is ready (so a DBus caller can never reach the server before the
  socket exists).
- **Shutdown:** the reactor's shutdown path sends on the oneshot channel,
  the DBus worker tears down its zbus connection, and `main.rs` joins the
  thread before exiting.
- **Reconnection to the SDK socket:** the SDK already handles reconnection;
  no special logic in `dbus.rs`.

## Concurrency

The DBus thread is single-threaded (`tokio::runtime::Builder::new_current_thread`).
zbus serializes method dispatch on the connection. Each method handler is
a short async function that issues an SDK call and awaits the response.
The SDK client is reused across calls (one connection to the socket for
the whole DBus thread's lifetime). No locks are required.

## Testing

Two test surfaces:

1. **Integration test** in `server/tests/dbus_smoke.rs`: launches a
   throwaway ringboard-server (in a temp `XDG_DATA_HOME`), starts a
   private DBus session via `dbus-launch` (or `zbus`'s test helper), calls
   each method through a zbus proxy, asserts the entries appear via the
   socket SDK. Skipped in CI environments where `dbus-launch` is missing.
2. **Manual smoke from busctl**:
   ```
   busctl --user call com.github.SUPERCILEX.Ringboard \
     /com/github/SUPERCILEX/Ringboard \
     com.github.SUPERCILEX.Ringboard1 Add \
     ays "hello" 5 text/plain
   ```
   Documented in a short `server/README.md` snippet.

No unit tests on `dbus.rs` itself — the interface methods are thin enough
that the integration test covers them.

## Forward-compatibility

The interface is versioned by its name suffix (`Ringboard1`). Any breaking
change becomes `Ringboard2` on a new object path; the old interface can
coexist.

## Out-of-scope, but anticipated

- `NewEntry` signal — straightforward follow-up once the reactor exposes a
  change broadcast channel. Tracked separately.
- `Last() → t` convenience method.
- DBus introspection XML maintained by hand if zbus's autogenerated XML
  drifts from convention. Default to zbus's emission.

## Rollout

1. Land this on `feat/dbus-interface` branched from `upstream/master`.
2. Verify with the `busctl` calls and the integration test.
3. Separate follow-up spec migrates the GNOME extension to use DBus
   instead of the CLI subprocess client.
