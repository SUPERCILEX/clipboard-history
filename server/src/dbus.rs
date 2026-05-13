#![cfg(feature = "dbus")]

use std::{
    collections::{BTreeSet, HashSet},
    fs::File,
    io::{Seek, SeekFrom, Write},
    os::fd::OwnedFd,
    sync::Arc,
    thread::{self, JoinHandle},
};

use log::{info, warn};
use ringboard_core::dirs::{data_dir, socket_file};
use ringboard_core::protocol::{AddResponse, RingKind};
use ringboard_sdk::{
    DatabaseReader, Entry, EntryReader, Kind,
    api::{AddRequest, MoveToFrontRequest, RemoveRequest, connect_to_server},
    core::{BucketAndIndex, size_to_bucket},
    search::{EntryLocation, Query, QueryResult, cancellation_token},
};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::net::SocketAddrUnix;
use zbus::connection::Builder;

pub const BUS_NAME: &str = "com.github.SUPERCILEX.Ringboard";
pub const OBJECT_PATH: &str = "/com/github/SUPERCILEX/Ringboard";
pub const INTERFACE_NAME: &str = "com.github.SUPERCILEX.Ringboard1";

pub const MAX_PAGE_LIMIT: u64 = 500;

// Convention for interface methods:
//
// The SDK is synchronous (rustix syscalls, blocking socket I/O), and the
// tokio runtime hosting zbus is single-threaded. To avoid blocking the
// dispatcher (and timing out heartbeats), interface methods must wrap
// their SDK work inside `tokio::task::spawn_blocking(...).await`. Don't
// .await SDK calls directly inside the method body.

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

fn open_server() -> zbus::fdo::Result<OwnedFd> {
    let sock = socket_file();
    let addr = SocketAddrUnix::new(&sock)
        .map_err(|e| zbus::fdo::Error::Failed(format!("invalid socket path: {e}")))?;
    connect_to_server(&addr)
        .map_err(|e| zbus::fdo::Error::Failed(format!("connect to server: {e}")))
}

fn payload_memfd(bytes: &[u8]) -> zbus::fdo::Result<File> {
    let fd = memfd_create(c"ringboard-dbus-add", MemfdFlags::CLOEXEC)
        .map_err(|e| zbus::fdo::Error::Failed(format!("memfd_create: {e}")))?;
    let mut file = File::from(fd);
    file.write_all(bytes)
        .map_err(|e| zbus::fdo::Error::Failed(format!("write payload: {e}")))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| zbus::fdo::Error::Failed(format!("seek payload: {e}")))?;
    Ok(file)
}

fn open_db() -> zbus::fdo::Result<(DatabaseReader, EntryReader)> {
    let mut dir = data_dir();
    let db = DatabaseReader::open(&mut dir)
        .map_err(|e| zbus::fdo::Error::Failed(format!("DatabaseReader::open: {e}")))?;
    let reader = EntryReader::open(&mut dir)
        .map_err(|e| zbus::fdo::Error::Failed(format!("EntryReader::open: {e}")))?;
    Ok((db, reader))
}

fn search_text(query: &str, db: &DatabaseReader) -> zbus::fdo::Result<Vec<Entry>> {
    let mut search_dir = data_dir();
    let search_reader = EntryReader::open(&mut search_dir)
        .map_err(|e| zbus::fdo::Error::Failed(format!("EntryReader::open: {e}")))?;
    let search_reader = Arc::new(search_reader);

    let (token_src, _token_sink) = cancellation_token();
    let (results, threads) =
        ringboard_sdk::search(Query::Plain(query.as_bytes()), search_reader, token_src);

    let mut file_ids: HashSet<u64> = HashSet::new();
    let mut bucket_hits: BTreeSet<BucketAndIndex> = BTreeSet::new();
    for r in results {
        let QueryResult { location, .. } =
            r.map_err(|e| zbus::fdo::Error::Failed(format!("search: {e}")))?;
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
        t.join()
            .map_err(|_| zbus::fdo::Error::Failed("search thread panicked".into()))?;
    }

    let mut out: Vec<Entry> = Vec::new();
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

fn load_row(
    entry: &Entry,
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

struct Iface;

#[zbus::interface(name = "com.github.SUPERCILEX.Ringboard1")]
impl Iface {
    /// Drop every entry from the server.
    async fn wipe(&self) -> zbus::fdo::Result<()> {
        tokio::task::spawn_blocking(|| -> zbus::fdo::Result<()> {
            let server = open_server()?;

            let mut database = data_dir();
            let db = DatabaseReader::open(&mut database)
                .map_err(|e| zbus::fdo::Error::Failed(format!("open database: {e}")))?;

            let ids: Vec<u64> = db
                .favorites()
                .chain(db.main())
                .map(|e| e.id())
                .collect();

            for id in ids {
                let _resp = RemoveRequest::response(&server, id)
                    .map_err(|e| zbus::fdo::Error::Failed(format!("remove {id}: {e}")))?;
            }

            Ok(())
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("wipe join: {e}")))??;
        Ok(())
    }

    /// Drop the entry with the given id.
    async fn remove(&self, id: u64) -> zbus::fdo::Result<()> {
        tokio::task::spawn_blocking(move || -> zbus::fdo::Result<()> {
            let server = open_server()?;
            let resp = RemoveRequest::response(&server, id)
                .map_err(|e| zbus::fdo::Error::Failed(format!("remove {id}: {e}")))?;
            if let Some(err) = resp.error {
                return Err(zbus::fdo::Error::Failed(format!("remove {id}: {err:?}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("remove join: {e}")))??;
        Ok(())
    }

    /// Move the entry with the given id to the front of the main ring.
    async fn move_to_front(&self, id: u64) -> zbus::fdo::Result<()> {
        tokio::task::spawn_blocking(move || -> zbus::fdo::Result<()> {
            let server = open_server()?;
            let resp = MoveToFrontRequest::response(&server, id, Some(RingKind::Main))
                .map_err(|e| zbus::fdo::Error::Failed(format!("move_to_front {id}: {e}")))?;
            match resp {
                ringboard_core::protocol::MoveToFrontResponse::Success { id: _ } => Ok(()),
                ringboard_core::protocol::MoveToFrontResponse::Error(e) => {
                    Err(zbus::fdo::Error::Failed(format!("move_to_front {id}: {e:?}")))
                }
            }
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("move_to_front join: {e}")))??;
        Ok(())
    }

    /// Append a new entry to the main ring. Returns the assigned id.
    async fn add(&self, payload: Vec<u8>, mime: &str) -> zbus::fdo::Result<u64> {
        if payload.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("empty payload".into()));
        }
        let mime = mime.to_owned();
        tokio::task::spawn_blocking(move || -> zbus::fdo::Result<u64> {
            let mime_type = ringboard_core::protocol::MimeType::from(&mime)
                .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("invalid mime: {e}")))?;
            let file = payload_memfd(&payload)?;
            let server = open_server()?;
            let resp = AddRequest::response(&server, RingKind::Main, &mime_type, &file)
                .map_err(|e| zbus::fdo::Error::Failed(format!("add: {e}")))?;
            let AddResponse::Success { id } = resp;
            Ok(id)
        })
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("add join: {e}")))?
    }

    /// Paginated search. Empty query lists every entry (favorites then main);
    /// non-empty query runs a plaintext search. Returns `(page, total)`.
    /// `limit` is clamped to `MAX_PAGE_LIMIT`; `limit == 0` returns an empty
    /// page along with the true total.
    async fn search(
        &self,
        query: &str,
        offset: u64,
        limit: u64,
    ) -> zbus::fdo::Result<(Vec<(u64, String, Vec<u8>)>, u64)> {
        let limit = limit.min(MAX_PAGE_LIMIT);
        let query = query.to_owned();

        tokio::task::spawn_blocking(
            move || -> zbus::fdo::Result<(Vec<(u64, String, Vec<u8>)>, u64)> {
                let (db, mut reader) = open_db()?;
                let entries: Vec<Entry> = if query.is_empty() {
                    db.favorites().chain(db.main()).collect()
                } else {
                    search_text(&query, &db)?
                };
                let total = entries.len() as u64;
                let start = offset.min(total) as usize;
                let end = offset.saturating_add(limit).min(total) as usize;

                let mut page = Vec::with_capacity(end.saturating_sub(start));
                for entry in &entries[start..end] {
                    page.push(load_row(entry, &mut reader)?);
                }
                Ok((page, total))
            },
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("search join: {e}")))?
    }
}

async fn serve() -> zbus::Result<()> {
    let _conn = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Iface)?
        .build()
        .await?;
    info!("DBus interface registered on session bus as {BUS_NAME}");
    // Park forever; zbus dispatches in the background.
    std::future::pending::<()>().await;
    Ok(())
}
