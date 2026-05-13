# Ringboard Server DBus Interface — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose the ringboard server's API on the session DBus so non-Rust clients can call it natively without spawning the CLI.

**Architecture:** A new module `server/src/dbus.rs` spawns a worker thread hosting a tokio current-thread runtime and a zbus connection. The thread links the existing `clipboard-history-client-sdk` to issue mutations through the server's own Unix socket and opens the ring files directly for queries — so `reactor.rs`, `requests.rs`, `io_uring.rs` are not modified. All new code lives behind a `dbus` cargo feature (default-on).

**Tech Stack:** Rust 2024 edition, tokio (current-thread), zbus 5.x, existing `clipboard-history-client-sdk`, `clipboard-history-core`.

**Spec:** `docs/superpowers/specs/2026-05-13-ringboard-server-dbus-interface-design.md`.

---

## File map

- **Create** `server/src/dbus.rs` — DBus interface object, bus name acquisition, worker thread loop.
- **Modify** `server/Cargo.toml` — add `dbus` feature, optional deps on `zbus` and `tokio`, pull `clipboard-history-client-sdk` with `search` feature.
- **Modify** `server/src/main.rs` — spawn the worker thread from `run()` after `claim_server_ownership()`.
- **Create** `server/tests/dbus_smoke.rs` — end-to-end integration test using a private DBus session and a server child process.
- **Modify** `server/README.md` — add a "DBus interface" section with `busctl` examples.

Bus identifiers (constants in `dbus.rs`):
- name `com.github.SUPERCILEX.Ringboard`
- path `/com/github/SUPERCILEX/Ringboard`
- interface `com.github.SUPERCILEX.Ringboard1`

DBus method signatures (final shape, used throughout):
| Method | In | Out |
|---|---|---|
| `Add` | `payload: ay`, `mime: s` | `id: t` |
| `Search` | `query: s`, `offset: t`, `limit: t` | `(page: a(tsay), total: t)` |
| `MoveToFront` | `id: t` | `()` |
| `Remove` | `id: t` | `()` |
| `Wipe` | `()` | `()` |

`limit` is clamped server-side to `MAX_PAGE_LIMIT = 500`. `limit == 0` returns an empty page plus the live `total`.

---

## Task 0: Create the branch

**Files:**
- None (git only)

- [ ] **Step 1: Fetch upstream and create branch**

```bash
git fetch upstream
git checkout -b feat/dbus-interface upstream/master
git log --oneline -1
```

Expected: HEAD at `86f94f9 Tweak install script app restart order` (or newer if upstream advanced — note the SHA you started from).

- [ ] **Step 2: Confirm no existing dbus code**

```bash
grep -rn 'dbus\|zbus' server/ client-sdk/ core/ cli/ 2>/dev/null
```

Expected: no output.

---

## Task 1: Cargo feature gate + empty module

Goal: the crate still compiles with and without the `dbus` feature, and `dbus.rs` exists as an empty placeholder.

**Files:**
- Modify: `server/Cargo.toml`
- Create: `server/src/dbus.rs`
- Modify: `server/src/main.rs`

- [ ] **Step 1: Add deps + feature in `server/Cargo.toml`**

Inside `[dependencies]`, append:

```toml
tokio = { version = "1.47", default-features = false, features = ["rt", "macros", "sync"], optional = true }
zbus = { version = "5.5", default-features = false, features = ["tokio"], optional = true }
```

Change the `ringboard-sdk` line so the `search` feature is enabled when `dbus` is on:

```toml
ringboard-sdk = { package = "clipboard-history-client-sdk", version = "0", path = "../client-sdk" }
```

(unchanged — the `search` feature gets pulled in via the `dbus` feature below.)

Replace the `[features]` block with:

```toml
[features]
default = ["systemd", "human-logs", "dbus"]
systemd = ["dep:sd-notify"]
human-logs = ["env_logger/default"]
dbus = ["dep:zbus", "dep:tokio", "ringboard-sdk/search"]
trace = ["dep:tracy-client"]
```

- [ ] **Step 2: Create empty module**

Create `server/src/dbus.rs`:

```rust
// DBus front-end for ringboard-server.
//
// The interface is hosted by a worker thread that runs a tokio
// current-thread runtime and a zbus connection. Mutations are issued
// through the server's Unix socket via the client-sdk; read queries open
// the ring files directly. The io_uring reactor in reactor.rs is not
// touched.

#![cfg(feature = "dbus")]

use std::thread::{self, JoinHandle};

/// Spawn the DBus worker. The returned `JoinHandle` is intentionally
/// dropped on shutdown — the thread is a daemon, and the process exiting
/// tears down the zbus connection cleanly.
#[allow(clippy::missing_errors_doc)]
pub fn spawn() -> JoinHandle<()> {
    thread::Builder::new()
        .name("ringboard-dbus".into())
        .spawn(|| {
            // Real worker arrives in Task 2.
        })
        .expect("failed to spawn ringboard-dbus thread")
}
```

- [ ] **Step 3: Register the module from `main.rs`**

In `server/src/main.rs`, just below the existing `mod startup;` line (around line 18), add:

```rust
#[cfg(feature = "dbus")]
mod dbus;
```

- [ ] **Step 4: Confirm both builds compile**

```bash
cargo build -p clipboard-history-server
cargo build -p clipboard-history-server --no-default-features --features systemd,human-logs
```

Expected: both succeed with no warnings.

- [ ] **Step 5: Commit**

```bash
git add server/Cargo.toml server/src/dbus.rs server/src/main.rs Cargo.lock
git commit -m "server: add dbus feature scaffolding"
```

---

## Task 2: Worker thread skeleton — claim bus name, idle

Goal: when the server starts with `dbus` enabled, a thread acquires the bus name and remains alive until the process exits. No interface methods yet.

**Files:**
- Modify: `server/src/dbus.rs`
- Modify: `server/src/main.rs`

- [ ] **Step 1: Replace `dbus.rs` body**

```rust
#![cfg(feature = "dbus")]

use std::thread::{self, JoinHandle};

use log::{info, warn};
use zbus::connection::Builder;

pub const BUS_NAME: &str = "com.github.SUPERCILEX.Ringboard";
pub const OBJECT_PATH: &str = "/com/github/SUPERCILEX/Ringboard";
pub const INTERFACE_NAME: &str = "com.github.SUPERCILEX.Ringboard1";

pub fn spawn() -> JoinHandle<()> {
    thread::Builder::new()
        .name("ringboard-dbus".into())
        .spawn(run)
        .expect("failed to spawn ringboard-dbus thread")
}

fn run() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for ringboard-dbus");

    if let Err(e) = rt.block_on(serve()) {
        warn!("ringboard-dbus exiting: {e}");
    }
}

async fn serve() -> zbus::Result<()> {
    let _conn = Builder::session()?
        .name(BUS_NAME)?
        .build()
        .await?;
    info!("DBus interface registered on session bus as {BUS_NAME}");
    // Park forever; zbus dispatches in the background.
    std::future::pending::<()>().await;
    Ok(())
}
```

- [ ] **Step 2: Spawn from `run()` in `main.rs`**

In `server/src/main.rs`, inside `run()` immediately after the `info!("Acquired server lock.");` line (around line 105):

```rust
    #[cfg(feature = "dbus")]
    let _dbus_handle = dbus::spawn();
```

The handle is bound to `_` to suppress unused warnings; the daemon thread dies with the process.

- [ ] **Step 3: Smoke check**

In one shell, launch a fresh server pointing at a temp data directory:

```bash
cargo build -p clipboard-history-server
RINGBOARD_DATABASE=$(mktemp -d) target/debug/ringboard-server &
SERVER_PID=$!
sleep 0.5
busctl --user list | grep -i Ringboard
```

Expected: a line like `com.github.SUPERCILEX.Ringboard ... <pid> ringboard-server ...`.

Then shut it down:

```bash
kill -INT $SERVER_PID
wait $SERVER_PID 2>/dev/null
```

- [ ] **Step 4: Commit**

```bash
git add server/src/dbus.rs server/src/main.rs Cargo.lock
git commit -m "server: spawn dbus worker thread and claim bus name"
```

---

## Task 3: Interface object + `Wipe`

We start with the simplest mutation method (no arguments, no return data) to validate the wiring before doing anything that needs an SDK connection.

**Files:**
- Modify: `server/src/dbus.rs`

- [ ] **Step 1: Add an SDK connection helper**

Add at the top of `server/src/dbus.rs` (under the use-block):

```rust
use std::os::fd::OwnedFd;

use ringboard_core::{dirs::socket_file, protocol::WipeRequest};
use ringboard_sdk::api::connect_to_server;
use rustix::net::SocketAddrUnix;

fn open_server() -> zbus::fdo::Result<OwnedFd> {
    let addr = SocketAddrUnix::new(&socket_file())
        .map_err(|e| zbus::fdo::Error::Failed(format!("invalid socket path: {e}")))?;
    connect_to_server(&addr)
        .map_err(|e| zbus::fdo::Error::Failed(format!("connect to server: {e}")))
}
```

If `WipeRequest` is not directly under `ringboard_core::protocol`, swap the import for wherever it lives — verify with:

```bash
grep -rn 'pub struct WipeRequest\|pub fn wipe' client-sdk/src/api.rs core/src/protocol.rs
```

and adjust the path; the body below uses the SDK's request type.

- [ ] **Step 2: Define the interface struct + register it**

Replace the `serve()` body in `dbus.rs` so it builds an `Iface` value and attaches it to the connection:

```rust
struct Iface;

#[zbus::interface(name = "com.github.SUPERCILEX.Ringboard1")]
impl Iface {
    /// Drop every entry from the server.
    async fn wipe(&self) -> zbus::fdo::Result<()> {
        let server = open_server()?;
        ringboard_sdk::api::WipeRequest::response(&server)
            .map_err(|e| zbus::fdo::Error::Failed(format!("wipe: {e}")))?;
        Ok(())
    }
}

async fn serve() -> zbus::Result<()> {
    let _conn = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Iface)?
        .build()
        .await?;
    info!("DBus interface registered on session bus as {BUS_NAME}");
    std::future::pending::<()>().await;
    Ok(())
}
```

Verify the exact `WipeRequest::response` signature with:

```bash
grep -n 'pub fn response' client-sdk/src/api.rs | grep -i wipe
```

If the response type is named differently or wants additional args, mirror what the CLI's wipe handler does (search `cli/src/main.rs` for `WipeRequest`).

- [ ] **Step 3: Compile**

```bash
cargo build -p clipboard-history-server
```

Expected: success.

- [ ] **Step 4: Smoke check**

Launch a clean server (see Task 2 step 3), then:

```bash
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Wipe
```

Expected: empty response, exit 0. Server still alive. The data directory's ring files are now empty/recreated.

- [ ] **Step 5: Commit**

```bash
git add server/src/dbus.rs
git commit -m "server: dbus Wipe method"
```

---

## Task 4: `Remove`

**Files:**
- Modify: `server/src/dbus.rs`

- [ ] **Step 1: Add the method to `Iface`**

Inside the `impl Iface` block:

```rust
    /// Drop the entry with the given id.
    async fn remove(&self, id: u64) -> zbus::fdo::Result<()> {
        let server = open_server()?;
        let resp = ringboard_sdk::api::RemoveRequest::response(&server, id)
            .map_err(|e| zbus::fdo::Error::Failed(format!("remove: {e}")))?;
        if let Some(err) = resp.error {
            return Err(zbus::fdo::Error::Failed(format!("remove: {err:?}")));
        }
        Ok(())
    }
```

Confirm `RemoveResponse`'s field layout — the CLI uses `let RemoveResponse { error } = ...`. If `error` is `Option<IdNotFoundError>` (or similar), keep the body above; if it's a plain enum, match it instead.

- [ ] **Step 2: Compile + smoke**

```bash
cargo build -p clipboard-history-server
```

Launch a clean server, add an entry via `ringboard add - <<< "to-remove"`, note the id printed, then:

```bash
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Remove t <ID>
```

Expected: empty response. `ringboard search '' --json | jq 'map(.id) | index(<ID>)'` returns `null`.

- [ ] **Step 3: Commit**

```bash
git add server/src/dbus.rs
git commit -m "server: dbus Remove method"
```

---

## Task 5: `MoveToFront`

**Files:**
- Modify: `server/src/dbus.rs`

- [ ] **Step 1: Add the method**

```rust
    /// Move the entry with the given id to the front of the main ring.
    async fn move_to_front(&self, id: u64) -> zbus::fdo::Result<()> {
        use ringboard_core::protocol::RingKind;
        let server = open_server()?;
        ringboard_sdk::api::MoveToFrontRequest::response(&server, id, Some(RingKind::Main))
            .map_err(|e| zbus::fdo::Error::Failed(format!("move_to_front: {e}")))?;
        Ok(())
    }
```

Verify the signature with:

```bash
grep -n 'fn response' client-sdk/src/api.rs | grep -A1 MoveToFrontRequest
sed -n '230,260p' client-sdk/src/api.rs
```

If the second argument is `RingKind` directly (not `Option<RingKind>`), drop the `Some(...)`.

- [ ] **Step 2: Smoke**

```bash
cargo build -p clipboard-history-server
# (relaunch server, add two entries 'a' and 'b'; note 'a's id)
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  MoveToFront t <ID_OF_a>
ringboard search '' --json | jq '.[0].data'
```

Expected: `"a"` is now the first entry.

- [ ] **Step 3: Commit**

```bash
git add server/src/dbus.rs
git commit -m "server: dbus MoveToFront method"
```

---

## Task 6: `Add`

The SDK's `AddRequest::response` takes a file descriptor for the payload. We materialise the DBus `ay` bytes in a `memfd_create` anonymous file before calling it.

**Files:**
- Modify: `server/src/dbus.rs`

- [ ] **Step 1: Add a memfd helper**

Append to `dbus.rs`:

```rust
use std::io::Write;
use rustix::fs::{MemfdFlags, memfd_create};

fn payload_memfd(bytes: &[u8]) -> zbus::fdo::Result<std::fs::File> {
    let fd = memfd_create(c"ringboard-dbus-add", MemfdFlags::CLOEXEC)
        .map_err(|e| zbus::fdo::Error::Failed(format!("memfd_create: {e}")))?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)
        .map_err(|e| zbus::fdo::Error::Failed(format!("write payload: {e}")))?;
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| zbus::fdo::Error::Failed(format!("seek payload: {e}")))?;
    Ok(file)
}
```

- [ ] **Step 2: Add the method**

```rust
    /// Append a new entry. Returns the assigned id.
    async fn add(&self, payload: Vec<u8>, mime: &str) -> zbus::fdo::Result<u64> {
        use ringboard_core::protocol::{AddResponse, MimeType, RingKind};
        if payload.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("empty payload".into()));
        }
        let mime_type = MimeType::from(mime)
            .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("invalid mime: {e}")))?;
        let file = payload_memfd(&payload)?;
        let server = open_server()?;
        let AddResponse::Success { id } = ringboard_sdk::api::AddRequest::response(
            &server,
            RingKind::Main,
            &mime_type,
            &file,
        )
        .map_err(|e| zbus::fdo::Error::Failed(format!("add: {e}")))?;
        Ok(id)
    }
```

Confirm `MimeType`'s construction API with:

```bash
grep -n 'impl.*MimeType\|TryFrom.*MimeType\|MimeType::from' core/src/protocol.rs client-sdk/src/api.rs | head
```

If it's `MimeType::try_from(...)` or `ArrayString::from(...)`, adjust accordingly. `MimeType` is `ArrayString<96>`, so `MimeType::from(mime).map_err(...)` matches the arrayvec API.

- [ ] **Step 3: Smoke**

```bash
cargo build -p clipboard-history-server
# relaunch server
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Add ays 5 104 101 108 108 111 "text/plain;charset=utf-8"
ringboard search '' --json | jq '.[0]'
```

Expected: returned `t <id>`, and `ringboard search` shows `{ "id": <id>, "data": "hello", "mime_type": "text/plain;charset=utf-8" }` at the top.

- [ ] **Step 4: Commit**

```bash
git add server/src/dbus.rs
git commit -m "server: dbus Add method"
```

---

## Task 7: `Search` (paginated, unified text/binary listing)

This is the largest method. Empty query → list every entry (favorites + main) newest-first. Non-empty query → text search. Both apply `offset`/`limit` to the same result-set shape `(id, mime, payload)`.

**Files:**
- Modify: `server/src/dbus.rs`

- [ ] **Step 1: Add the constant + a helper that opens the database**

```rust
use std::{borrow::Cow, sync::Arc};

use ringboard_core::dirs::data_dir;
use ringboard_sdk::{
    DatabaseReader, EntryReader,
    search::{CaselessQuery, Query, search as sdk_search},
};
use ringboard_sdk::core::IoErr;

pub const MAX_PAGE_LIMIT: u64 = 500;

fn open_db() -> zbus::fdo::Result<(DatabaseReader, EntryReader)> {
    let mut dir = data_dir();
    if !dir
        .try_exists()
        .map_io_err(|| format!("database existence: {dir:?}"))
        .map_err(|e| zbus::fdo::Error::Failed(format!("open_db: {e}")))?
    {
        return Err(zbus::fdo::Error::Failed(format!("database not found at {dir:?}")));
    }
    let db = DatabaseReader::open(&mut dir)
        .map_err(|e| zbus::fdo::Error::Failed(format!("DatabaseReader::open: {e}")))?;
    let reader = EntryReader::open(&mut dir)
        .map_err(|e| zbus::fdo::Error::Failed(format!("EntryReader::open: {e}")))?;
    Ok((db, reader))
}
```

- [ ] **Step 2: A helper that loads one entry into a `(u64, String, Vec<u8>)` tuple**

```rust
fn load_row(
    entry: &ringboard_sdk::Entry,
    reader: &mut EntryReader,
) -> zbus::fdo::Result<(u64, String, Vec<u8>)> {
    let bytes = entry
        .to_slice(reader)
        .map_err(|e| zbus::fdo::Error::Failed(format!("load entry: {e}")))?;
    let mime = bytes
        .mime_type()
        .map_err(|e| zbus::fdo::Error::Failed(format!("mime: {e}")))?
        .to_string();
    Ok((entry.id(), mime, bytes.to_vec()))
}
```

- [ ] **Step 3: Add the `search` method**

```rust
    /// Paginated search. Empty query lists every entry (favorites then
    /// main) newest-first; non-empty query runs a text search. Returns
    /// `(page, total)` where `page` is the slice `[offset, offset+limit)`.
    /// `limit` is clamped to MAX_PAGE_LIMIT.
    async fn search(
        &self,
        query: &str,
        offset: u64,
        limit: u64,
    ) -> zbus::fdo::Result<(Vec<(u64, String, Vec<u8>)>, u64)> {
        let limit = limit.min(MAX_PAGE_LIMIT);

        // open_db touches the filesystem; do it on the blocking pool so we
        // don't stall the dbus dispatcher on cold caches.
        let (db, mut reader) = tokio::task::spawn_blocking(open_db)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("join: {e}")))??;

        let entries: Vec<ringboard_sdk::Entry> = if query.is_empty() {
            // Newest-first: main is naturally newest-first via reverse iter
            // (matches RingReader semantics); favorites come first because
            // dump() does favorites.chain(main) — keep that order so the
            // GNOME extension's existing assumption holds.
            db.favorites().chain(db.main()).collect()
        } else {
            search_text(query, &db, &mut reader)?
        };

        let total = entries.len() as u64;
        let start = offset.min(total) as usize;
        let end = (offset.saturating_add(limit)).min(total) as usize;

        let mut page = Vec::with_capacity(end - start);
        for entry in &entries[start..end] {
            page.push(load_row(entry, &mut reader)?);
        }
        Ok((page, total))
    }
```

- [ ] **Step 4: Implement `search_text`**

`ringboard_sdk::search` requires `Arc<EntryReader>`; rather than juggling
the same reader between the search call and the post-search `load_row`,
open a second short-lived reader for the search and let it drop when the
function returns. The cost is one extra mmap dance per non-empty-query
call, which is negligible compared to the search itself.

The SDK reports two kinds of matches:
- `EntryLocation::File { entry_id }` — large/direct entries; the id is
  already known.
- `EntryLocation::Bucketed { bucket, index }` — small entries (most
  clipboard text) stored in size-class buckets; the entry id is resolved
  by iterating `db.favorites().chain(db.main())` and looking up
  `BucketAndIndex::new(size_to_bucket(bucket.size()), bucket.index())`.

We need both — skipping bucketed matches would hide the majority of text
search hits. Mirror the CLI's reconciliation.

```rust
fn search_text<'db>(
    query: &str,
    db: &'db DatabaseReader,
) -> zbus::fdo::Result<Vec<ringboard_sdk::Entry<'db>>> {
    use std::collections::{BTreeSet, HashSet};

    use ringboard_core::{BucketAndIndex, size_to_bucket};
    use ringboard_sdk::{Kind, search::{EntryLocation, QueryResult}};

    let mut search_dir = data_dir();
    let search_reader = EntryReader::open(&mut search_dir)
        .map_err(|e| zbus::fdo::Error::Failed(format!("EntryReader::open: {e}")))?;
    let search_reader = Arc::new(search_reader);

    let (token_src, _token_sink) = ringboard_sdk::search::cancellation_token();
    let (results, threads) = sdk_search(
        Query::Plain(query.as_bytes()),
        search_reader,
        token_src,
    );

    let mut file_ids: HashSet<u64> = HashSet::new();
    let mut bucket_hits: BTreeSet<BucketAndIndex> = BTreeSet::new();
    for r in results {
        let QueryResult { location, .. } = r
            .map_err(|e| zbus::fdo::Error::Failed(format!("search: {e}")))?;
        match location {
            EntryLocation::File { entry_id } => {
                file_ids.insert(entry_id);
            }
            EntryLocation::Bucketed { bucket, index } => {
                bucket_hits.insert(BucketAndIndex::new(bucket, index));
            }
        }
    }
    for t in threads {
        t.join().map_err(|_| zbus::fdo::Error::Failed("search thread panicked".into()))?;
    }

    let mut out: Vec<ringboard_sdk::Entry<'db>> = Vec::new();
    for entry in db.favorites().chain(db.main()) {
        let hit = match entry.kind() {
            Kind::File => file_ids.contains(&entry.id()),
            Kind::Bucket(b) => bucket_hits.contains(&BucketAndIndex::new(
                size_to_bucket(b.size()),
                b.index(),
            )),
        };
        if hit {
            out.push(entry);
        }
    }
    Ok(out)
}
```

The `Entry<'db>` lifetime borrows `db`; the caller (`search` method)
keeps `db` alive on its stack. If the SDK names the enum variants
differently (e.g. `Kind::Bucket` vs `Kind::Bucketed`), follow the
compiler error — the imports `Kind`, `BucketAndIndex`, `size_to_bucket`
and `EntryLocation` are real and reachable through `ringboard_sdk` and
`ringboard_core`.

- [ ] **Step 5: Compile**

```bash
cargo build -p clipboard-history-server
```

Fix any lifetime/borrow issues raised by the compiler — `ringboard_sdk::Entry` borrows from `DatabaseReader`; keep `db` alive for the duration of the slice.

- [ ] **Step 6: Smoke**

```bash
# relaunch server, populate a few entries
for s in alpha beta gamma delta epsilon; do ringboard add - <<< "$s"; done

busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Search stt "" 0 2
```

Expected: a structure containing an array of two `(t, s, ay)` tuples (the two newest entries — `epsilon`, `delta`) and a `t` total of `5`.

```bash
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Search stt "alp" 0 50
```

Expected: one tuple (`alpha`) and total `1`.

```bash
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Search stt "" 0 0
```

Expected: empty page, total `5`.

- [ ] **Step 7: Commit**

```bash
git add server/src/dbus.rs
git commit -m "server: dbus Search method with offset/limit pagination"
```

---

## Task 8: Integration test

End-to-end smoke that launches a private bus, starts the server with a temp data dir, calls each DBus method through a zbus proxy, and asserts behaviour. Skipped if `dbus-launch` isn't on PATH (CI sandboxes often lack it).

**Files:**
- Create: `server/tests/dbus_smoke.rs`

- [ ] **Step 1: Write the test**

Create `server/tests/dbus_smoke.rs`:

```rust
#![cfg(feature = "dbus")]

use std::{
    env,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use tokio::time::sleep;
use zbus::{Connection, Proxy};

const BUS_NAME: &str = "com.github.SUPERCILEX.Ringboard";
const OBJECT_PATH: &str = "/com/github/SUPERCILEX/Ringboard";
const INTERFACE: &str = "com.github.SUPERCILEX.Ringboard1";

fn binary_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push("target");
    p.push(if cfg!(debug_assertions) { "debug" } else { "release" });
    p.push("ringboard-server");
    p
}

struct Bus {
    addr: String,
    pid: u32,
    _server: Child,
    _tmpdir: tempfile::TempDir,
}

fn start_bus_and_server() -> Option<Bus> {
    if which::which("dbus-launch").is_err() {
        return None;
    }
    let output = Command::new("dbus-launch").arg("--sh-syntax").output().ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut addr = None;
    let mut pid = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_ADDRESS='") {
            addr = rest.strip_suffix("';").map(|s| s.to_owned());
        } else if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_PID=") {
            pid = rest.trim_end_matches(';').parse::<u32>().ok();
        }
    }
    let addr = addr?;
    let pid = pid?;

    let tmpdir = tempfile::tempdir().ok()?;
    let server = Command::new(binary_path())
        .env("XDG_DATA_HOME", tmpdir.path())
        .env("DBUS_SESSION_BUS_ADDRESS", &addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    Some(Bus { addr, pid, _server: server, _tmpdir: tmpdir })
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self._server.kill();
        let _ = nix_kill(self.pid);
    }
}

fn nix_kill(pid: u32) -> std::io::Result<()> {
    Command::new("kill").arg(pid.to_string()).status()?;
    Ok(())
}

async fn proxy(addr: &str) -> Proxy<'static> {
    let conn = Connection::session_with_address(addr).await.unwrap();
    Proxy::new(&conn, BUS_NAME, OBJECT_PATH, INTERFACE).await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn add_search_remove_roundtrip() {
    let Some(bus) = start_bus_and_server() else {
        eprintln!("dbus-launch not available; skipping");
        return;
    };
    // Give the server a moment to claim the name.
    sleep(Duration::from_millis(500)).await;
    let p = proxy(&bus.addr).await;

    let id: u64 = p
        .call("Add", &(b"hello".as_slice(), "text/plain"))
        .await
        .unwrap();
    let (page, total): (Vec<(u64, String, Vec<u8>)>, u64) =
        p.call("Search", &("", 0u64, 50u64)).await.unwrap();
    assert_eq!(total, 1, "total should be 1");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].0, id);
    assert_eq!(page[0].2, b"hello");

    let (page2, _): (Vec<(u64, String, Vec<u8>)>, u64) =
        p.call("Search", &("ell", 0u64, 50u64)).await.unwrap();
    assert_eq!(page2.len(), 1);

    p.call::<_, ()>("Remove", &id).await.unwrap();
    let (_, total_after): (Vec<(u64, String, Vec<u8>)>, u64) =
        p.call("Search", &("", 0u64, 50u64)).await.unwrap();
    assert_eq!(total_after, 0);
}
```

- [ ] **Step 2: Add test deps**

In `server/Cargo.toml`, under `[dev-dependencies]`:

```toml
[dev-dependencies]
tempfile = "3.13"
tokio = { version = "1.47", features = ["macros", "rt", "time"] }
which = "7.0"
zbus = { version = "5.5", default-features = false, features = ["tokio"] }
```

(If `[dev-dependencies]` doesn't exist yet, create it.)

- [ ] **Step 3: Build the server release-or-debug binary that the test expects**

```bash
cargo build -p clipboard-history-server
```

The test reads `target/debug/ringboard-server`.

- [ ] **Step 4: Run the test**

```bash
cargo test -p clipboard-history-server --test dbus_smoke -- --nocapture
```

Expected: one test passes (or prints "dbus-launch not available; skipping" and exits 0).

- [ ] **Step 5: Commit**

```bash
git add server/tests/dbus_smoke.rs server/Cargo.toml Cargo.lock
git commit -m "server: integration test for dbus interface"
```

---

## Task 9: README + busctl examples

**Files:**
- Modify: `server/README.md`

- [ ] **Step 1: Append a section**

Open `server/README.md` and append (creating the file if it doesn't exist):

```markdown
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
(text + binary, favorites then main, newest-first). Build with
`--no-default-features --features systemd,human-logs` to disable.

Example:

```sh
busctl --user call \
  com.github.SUPERCILEX.Ringboard \
  /com/github/SUPERCILEX/Ringboard \
  com.github.SUPERCILEX.Ringboard1 \
  Add ays 5 104 101 108 108 111 "text/plain;charset=utf-8"
```
```

- [ ] **Step 2: Commit**

```bash
git add server/README.md
git commit -m "server: document dbus interface in README"
```

---

## Final verification

- [ ] **All tasks complete: run the full test suite for the server crate**

```bash
cargo test -p clipboard-history-server
cargo build -p clipboard-history-server
cargo build -p clipboard-history-server --no-default-features --features systemd,human-logs
cargo clippy -p clipboard-history-server --all-targets -- -D warnings
```

Expected: all green.

- [ ] **Push the branch**

```bash
git push -u origin feat/dbus-interface
```
