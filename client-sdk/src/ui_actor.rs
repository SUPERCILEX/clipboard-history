use std::{
    array,
    cmp::min,
    collections::{BinaryHeap, HashMap},
    hash::BuildHasherDefault,
    io::{BufReader, IoSlice},
    iter::once,
    mem,
    os::fd::{AsFd, OwnedFd},
    str,
    sync::Arc,
};

use image::{DynamicImage, ImageError, ImageReader};
use regex::bytes::Regex;
use ringboard_core::dirs::paste_socket_file;
use rustc_hash::FxHasher;
use rustix::net::{
    sendmsg_unix, socket_with, AddressFamily, SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
    SocketAddrUnix, SocketFlags, SocketType,
};
use thiserror::Error;

use crate::{
    api::{connect_to_server, MoveToFrontRequest, RemoveRequest},
    core::{
        dirs::{data_dir, socket_file},
        protocol::{composite_id, IdNotFoundError, MoveToFrontResponse, RemoveResponse, RingKind},
        ring::{Ring, MAX_ENTRIES},
        size_to_bucket, BucketAndIndex, Error as CoreError, IoErr, RingAndIndex,
    },
    search,
    search::{CancellationToken, CaselessQuery, EntryLocation, Query},
    ClientError, DatabaseReader, Entry, EntryReader, Kind,
};

#[derive(Error, Debug)]
pub enum CommandError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Sdk(#[from] ClientError),
    #[error("invalid RegEx")]
    Regex(#[from] regex::Error),
    #[error("failed to load image")]
    Image(#[from] ImageError),
}

impl From<IdNotFoundError> for CommandError {
    fn from(value: IdNotFoundError) -> Self {
        Self::Core(CoreError::IdNotFound(value))
    }
}

#[cfg(feature = "error-stack")]
mod error_stack_compat {
    use error_stack::{Context, Report};

    use super::CommandError;

    impl CommandError {
        pub fn into_report<W: Context>(self, wrapper: W) -> Report<W> {
            match self {
                Self::Core(e) => e.into_report(wrapper),
                Self::Sdk(e) => e.into_report(wrapper),
                Self::Regex(e) => Report::new(e).change_context(wrapper),
                Self::Image(e) => Report::new(e).change_context(wrapper),
            }
        }
    }
}

#[derive(Debug)]
pub enum Command {
    LoadFirstPage,
    GetDetails { id: u64, with_text: bool },
    Favorite(u64),
    Unfavorite(u64),
    Delete(u64),
    Search { query: Box<str>, kind: SearchKind },
    LoadImage(u64),
    Paste(u64),
}

#[derive(Default, Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum SearchKind {
    #[default]
    Plain,
    Regex,
    Mime,
}

#[derive(Debug)]
pub enum Message {
    FatalDbOpen(CoreError),
    Error(CommandError),
    LoadedFirstPage {
        entries: Box<[UiEntry]>,
        default_focused_id: Option<u64>,
    },
    EntryDetails {
        id: u64,
        result: Result<DetailedEntry, CoreError>,
    },
    PendingSearch(CancellationToken),
    SearchResults(Box<[UiEntry]>),
    FavoriteChange(u64),
    Deleted(u64),
    LoadedImage {
        id: u64,
        image: DynamicImage,
    },
    Pasted,
}

#[derive(Debug)]
pub struct UiEntry {
    pub entry: Entry,
    pub cache: UiEntryCache,
}

#[derive(Debug)]
pub enum UiEntryCache {
    Text { one_liner: Box<str> },
    Image,
    Binary { mime_type: Box<str> },
    Error(CoreError),
}

#[derive(Debug)]
pub struct DetailedEntry {
    pub mime_type: Box<str>,
    pub full_text: Option<Box<str>>,
}

pub fn controller<E>(
    commands: impl IntoIterator<Item = Command>,
    mut send: impl FnMut(Message) -> Result<(), E>,
) {
    fn maybe_init_server(cache: &mut Option<OwnedFd>) -> Result<impl AsFd + '_, ClientError> {
        if cache.is_some() {
            return Ok(cache.as_ref().unwrap());
        }

        let server = {
            let socket_file = socket_file();
            let addr = SocketAddrUnix::new(&socket_file)
                .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?;
            connect_to_server(&addr)?
        };

        Ok(cache.insert(server))
    }

    fn maybe_init_paste_server(
        cache: &mut Option<(OwnedFd, SocketAddrUnix)>,
    ) -> Result<(impl AsFd + '_, &SocketAddrUnix), ClientError> {
        if cache.is_some() {
            let (sock, addr) = cache.as_ref().unwrap();
            return Ok((sock, addr));
        }

        let addr = {
            let socket_file = paste_socket_file();
            SocketAddrUnix::new(&socket_file)
                .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?
        };
        let sock = socket_with(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::empty(),
            None,
        )
        .map_io_err(|| format!("Failed to create socket: {addr:?}"))?;

        let (sock, addr) = cache.insert((sock, addr));
        Ok((sock, addr))
    }

    let mut server = None;
    let mut paste_server = None;
    let (mut database, reader) = {
        let run = || {
            let mut dir = data_dir();

            let database = DatabaseReader::open(&mut dir)?;
            let reader = EntryReader::open(&mut dir)?;

            Ok((database, reader))
        };

        match run() {
            Ok(db) => db,
            Err(e) => {
                let _ = send(Message::FatalDbOpen(e));
                return;
            }
        }
    };
    let mut reader = Some(reader);
    let mut cache = Default::default();

    for command in once(Command::LoadFirstPage).chain(commands) {
        let result = handle_command(
            command,
            || maybe_init_server(&mut server),
            || maybe_init_paste_server(&mut paste_server),
            &mut send,
            &mut database,
            &mut reader,
            &mut cache,
        )
        .unwrap_or_else(|e| Some(Message::Error(e)));

        let Some(response) = result else {
            continue;
        };
        if send(response).is_err() {
            break;
        }
    }
}

fn handle_command<'a, Server: AsFd, PasteServer: AsFd, E>(
    command: Command,
    server: impl FnOnce() -> Result<Server, ClientError>,
    paste_server: impl FnOnce() -> Result<(PasteServer, &'a SocketAddrUnix), ClientError>,
    send: impl FnMut(Message) -> Result<(), E>,
    database: &mut DatabaseReader,
    reader_: &mut Option<EntryReader>,
    cache: &mut (
        Option<(u32, u32)>,
        HashMap<BucketAndIndex, RingAndIndex, BuildHasherDefault<FxHasher>>,
        Vec<RingAndIndex>,
    ),
) -> Result<Option<Message>, CommandError> {
    let reader = reader_.as_mut().unwrap();
    match command {
        Command::LoadFirstPage => {
            // This will trigger every time once the ring has reached capacity and doesn't
            // work if the ring fully wrapped around while we weren't looking.
            let shitty_refresh = |ring: &mut Ring| {
                let head = ring.write_head();
                #[allow(clippy::comparison_chain)]
                if head < ring.len() {
                    unsafe {
                        ring.set_len(ring.capacity());
                    }
                } else if head > ring.len() {
                    unsafe {
                        ring.set_len(head);
                    }
                }
            };
            shitty_refresh(database.favorites_ring_mut());
            shitty_refresh(database.main_ring_mut());

            let mut entries = Vec::with_capacity(100);
            for entry in database
                .favorites()
                .rev()
                .chain(database.main().rev().take(100))
            {
                entries.push(ui_entry(entry, reader).unwrap_or_else(|e| UiEntry {
                    cache: UiEntryCache::Error(e),
                    entry,
                }));
            }
            Ok(Some(Message::LoadedFirstPage {
                entries: entries.into(),
                default_focused_id: {
                    let mut main = database.main().rev();
                    let first = main.next();
                    main.next()
                        .or(first)
                        .or_else(|| database.favorites().next_back())
                        .as_ref()
                        .map(Entry::id)
                },
            }))
        }
        Command::GetDetails { id, with_text } => {
            let mut run = || {
                let entry = unsafe { database.get(id)? };
                if with_text {
                    let loaded = entry.to_slice(reader)?;
                    Ok(DetailedEntry {
                        mime_type: (&*loaded.mime_type()?).into(),
                        full_text: str::from_utf8(&loaded).map(Box::from).ok(),
                    })
                } else {
                    Ok(DetailedEntry {
                        mime_type: (&*entry.mime_type(reader)?).into(),
                        full_text: None,
                    })
                }
            };
            Ok(Some(Message::EntryDetails { id, result: run() }))
        }
        ref c @ (Command::Favorite(id) | Command::Unfavorite(id)) => {
            match MoveToFrontRequest::response(
                server()?,
                id,
                Some(match c {
                    Command::Favorite(_) => RingKind::Favorites,
                    Command::Unfavorite(_) => RingKind::Main,
                    _ => unreachable!(),
                }),
            )? {
                MoveToFrontResponse::Success { id } => Ok(Some(Message::FavoriteChange(id))),
                MoveToFrontResponse::Error(e) => Err(e.into()),
            }
        }
        Command::Delete(id) => match RemoveRequest::response(server()?, id)? {
            RemoveResponse { error: None } => Ok(Some(Message::Deleted(id))),
            RemoveResponse { error: Some(e) } => Err(e.into()),
        },
        Command::Search { query, kind } => {
            let query = match kind {
                SearchKind::Plain => {
                    if query
                        .chars()
                        .all(|c| !char::is_alphabetic(c) || char::is_lowercase(c))
                    {
                        Query::PlainIgnoreCase(CaselessQuery::new(query.into_boxed_bytes()).trim())
                    } else {
                        Query::Plain(query.trim().as_bytes())
                    }
                }
                SearchKind::Regex => Query::Regex(Regex::new(&query)?),
                SearchKind::Mime => Query::Mimes(Regex::new(&query)?),
            };
            Ok(Some(Message::SearchResults(
                do_search(query, reader_, database, send, cache).into(),
            )))
        }
        Command::LoadImage(id) => {
            let entry = unsafe { database.get(id)? };
            Ok(Some(Message::LoadedImage {
                id,
                image: ImageReader::new(BufReader::new(&*entry.to_file(reader)?))
                    .with_guessed_format()
                    .map_io_err(|| format!("Failed to guess image format for entry {id}."))?
                    .decode()?,
            }))
        }
        Command::Paste(id) => {
            let entry = unsafe { database.get(id)? };
            let (paste_server, addr) = paste_server()?;
            send_paste_buffer(paste_server, addr, entry, reader)?;
            Ok(Some(Message::Pasted))
        }
    }
}

fn ui_entry(entry: Entry, reader: &mut EntryReader) -> Result<UiEntry, CoreError> {
    let loaded = entry.to_slice(reader)?;
    let mime_type = &*loaded.mime_type()?;
    if mime_type.starts_with("image/") {
        return Ok(UiEntry {
            entry,
            cache: UiEntryCache::Image,
        });
    }

    Ok(match str::from_utf8(&loaded[..min(loaded.len(), 250)]) {
        Ok(s) => Some(s),
        Err(e) if e.error_len().is_none() => {
            Some(unsafe { str::from_utf8_unchecked(&loaded[..e.valid_up_to()]) })
        }
        Err(_) => None,
    }
    .map_or_else(
        || UiEntry {
            entry,
            cache: UiEntryCache::Binary {
                mime_type: mime_type.into(),
            },
        },
        |s| {
            let mut one_liner = String::new();
            let mut prev_char_is_whitespace = false;
            for c in s.chars() {
                if (prev_char_is_whitespace || one_liner.is_empty()) && c.is_whitespace() {
                    continue;
                }

                one_liner.push(if c.is_whitespace() { ' ' } else { c });
                prev_char_is_whitespace = c.is_whitespace();
            }
            if s.len() != loaded.len() {
                one_liner.push('…');
            }

            UiEntry {
                entry,
                cache: UiEntryCache::Text {
                    one_liner: one_liner.into(),
                },
            }
        },
    ))
}

fn do_search<E>(
    query: Query,
    reader_: &mut Option<EntryReader>,
    database: &mut DatabaseReader,
    mut send: impl FnMut(Message) -> Result<(), E>,
    (cached_write_heads, reverse_index_cache, search_result_buf): &mut (
        Option<(u32, u32)>,
        HashMap<BucketAndIndex, RingAndIndex, BuildHasherDefault<FxHasher>>,
        Vec<RingAndIndex>,
    ),
) -> Vec<UiEntry> {
    const MAX_SEARCH_ENTRIES: usize = 256;

    let reader = Arc::new(reader_.take().unwrap());

    let (result_stream, threads) = search(query, reader.clone());
    let _ = send(Message::PendingSearch(
        result_stream.cancellation_token().clone(),
    ));

    if *cached_write_heads
        != Some((
            database.favorites().ring().write_head(),
            database.main().ring().write_head(),
        ))
    {
        reverse_index_cache.clear();
    }
    if reverse_index_cache.is_empty() {
        for entry in database.favorites().chain(database.main()) {
            let Kind::Bucket(bucket) = entry.kind() else {
                continue;
            };
            reverse_index_cache.insert(
                BucketAndIndex::new(size_to_bucket(bucket.size()), bucket.index()),
                RingAndIndex::new(entry.ring(), entry.index()),
            );
        }
    }

    let mut results = BinaryHeap::from(mem::take(search_result_buf));
    let write_heads: [_; 2] = array::from_fn(|i| {
        let ring = if i == RingKind::Main as usize {
            database.main()
        } else if i == RingKind::Favorites as usize {
            database.favorites()
        } else {
            unreachable!()
        };
        let ring = ring.ring();
        ring.prev_entry(ring.write_head())
    });
    for entry in result_stream
        .flatten()
        .flat_map(|q| match q.location {
            EntryLocation::Bucketed { bucket, index } => reverse_index_cache
                .get(&BucketAndIndex::new(bucket, index))
                .copied()
                .ok_or_else(|| {
                    CoreError::IdNotFound(IdNotFoundError::Entry(
                        index << u8::BITS | u32::from(bucket),
                    ))
                }),
            EntryLocation::File { entry_id } => {
                RingAndIndex::from_id(entry_id).map_err(CoreError::IdNotFound)
            }
        })
        .map(|entry| {
            RingAndIndex::new(
                entry.ring(),
                write_heads[entry.ring() as usize].wrapping_sub(entry.index()) & MAX_ENTRIES,
            )
        })
    {
        if results.len() == MAX_SEARCH_ENTRIES {
            if entry < *results.peek().unwrap() {
                results.pop();
                results.push(entry);
            }
        } else {
            results.push(entry);
        }
    }

    for thread in threads {
        let _ = thread.join();
    }
    let reader = reader_.insert(Arc::into_inner(reader).unwrap());

    let mut results = results.into_vec();
    results.sort_unstable();
    #[allow(clippy::iter_with_drain)] // https://github.com/rust-lang/rust-clippy/issues/8539
    let entries = results
        .drain(..)
        .flat_map(|entry| {
            let ring = entry.ring();
            let index = write_heads[ring as usize].wrapping_sub(entry.index()) & MAX_ENTRIES;

            let id = composite_id(ring, index);
            unsafe { database.get(id) }
        })
        .map(|entry| {
            // TODO add support for bold highlighting the selection range
            ui_entry(entry, reader).unwrap_or_else(|e| UiEntry {
                cache: UiEntryCache::Error(e),
                entry,
            })
        })
        .collect();
    *search_result_buf = results;
    entries
}

fn send_paste_buffer(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    entry: Entry,
    reader: &mut EntryReader,
) -> ringboard_core::Result<()> {
    let file = entry.to_file(reader)?;
    let mime = file.mime_type()?;

    let mut space = [0; rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let fds = [file.as_fd()];
    {
        let success = ancillary.push(SendAncillaryMessage::ScmRights(&fds));
        debug_assert!(success);
    }
    sendmsg_unix(
        server,
        addr,
        &[IoSlice::new(mime.as_bytes())],
        &mut ancillary,
        SendFlags::empty(),
    )
    .map_io_err(|| format!("Failed to send paste entry to paste server at {addr:?}."))?;
    Ok(())
}
