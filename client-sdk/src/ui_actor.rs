use std::{
    array,
    cmp::{Ordering, min},
    collections::{BinaryHeap, HashMap},
    hash::BuildHasherDefault,
    io::BufReader,
    iter::once,
    mem,
    os::fd::{AsFd, OwnedFd},
    path::PathBuf,
    str,
    sync::Arc,
};

use image::{DynamicImage, ImageError, ImageReader};
use regex::bytes::Regex;
use ringboard_core::dirs::paste_socket_file;
use rustc_hash::FxHasher;
use rustix::net::SocketAddrUnix;
use thiserror::Error;

use crate::{
    ClientError, DatabaseReader, Entry, EntryReader, Kind,
    api::{
        MoveToFrontRequest, RemoveRequest, connect_to_paste_server, connect_to_server,
        send_paste_buffer,
    },
    core::{
        BucketAndIndex, Error as CoreError, IoErr, RingAndIndex,
        dirs::{data_dir, socket_file},
        protocol::{IdNotFoundError, MoveToFrontResponse, RemoveResponse, RingKind, composite_id},
        ring::{MAX_ENTRIES, Ring},
        size_to_bucket,
    },
    search,
    search::{CancellationToken, CaselessQuery, EntryLocation, Query, QueryResult},
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
    Text {
        one_liner: Box<str>,
    },
    HighlightedText {
        one_liner: Box<str>,
        start: usize,
        end: usize,
    },
    Image,
    Binary {
        mime_type: Box<str>,
    },
    Error(CoreError),
}

impl UiEntryCache {
    #[must_use]
    pub const fn is_text(&self) -> bool {
        match self {
            Self::Text { .. } | Self::HighlightedText { .. } => true,
            Self::Image | Self::Binary { .. } | Self::Error(_) => false,
        }
    }
}

#[derive(Debug)]
pub struct DetailedEntry {
    pub mime_type: Box<str>,
    pub full_text: Option<Box<str>>,
}

fn maybe_init_server(
    socket_file: impl FnOnce() -> PathBuf,
    connect_to_server: impl FnOnce(&SocketAddrUnix) -> Result<OwnedFd, ClientError>,
    cache: &mut Option<OwnedFd>,
) -> Result<impl AsFd + '_, ClientError> {
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

pub fn controller<E>(
    commands: impl IntoIterator<Item = Command>,
    mut send: impl FnMut(Message) -> Result<(), E>,
) {
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
            &mut server,
            &mut paste_server,
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

fn handle_command<E>(
    command: Command,
    server: &mut Option<OwnedFd>,
    paste_server: &mut Option<OwnedFd>,
    send: impl FnMut(Message) -> Result<(), E>,
    database: &mut DatabaseReader,
    reader_: &mut Option<EntryReader>,
    cache: &mut SearchCache,
) -> Result<Option<Message>, CommandError> {
    let shitty_refresh = |database: &mut DatabaseReader| {
        let run = |ring: &mut Ring| {
            let head = ring.write_head();
            #[allow(clippy::comparison_chain)]
            // This will trigger every time once the ring has reached capacity and doesn't
            // work if the ring fully wrapped around while we weren't looking.
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

        run(database.favorites_ring_mut());
        run(database.main_ring_mut());
    };

    let reader = reader_.as_mut().unwrap();
    match command {
        Command::LoadFirstPage => {
            shitty_refresh(database);

            let mut entries = Vec::with_capacity(100);
            for entry in database
                .favorites()
                .rev()
                .chain(database.main().rev().take(100))
            {
                entries.push(ui_entry(entry, reader, None));
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
            match {
                MoveToFrontRequest::response(
                    maybe_init_server(socket_file, connect_to_server, server)?,
                    id,
                    Some(match c {
                        Command::Favorite(_) => RingKind::Favorites,
                        Command::Unfavorite(_) => RingKind::Main,
                        _ => unreachable!(),
                    }),
                )
            }
            .inspect_err(|_| *server = None)?
            {
                MoveToFrontResponse::Success { id } => Ok(Some(Message::FavoriteChange(id))),
                MoveToFrontResponse::Error(e) => Err(e.into()),
            }
        }
        Command::Delete(id) => match {
            RemoveRequest::response(
                maybe_init_server(socket_file, connect_to_server, server)?,
                id,
            )
        }
        .inspect_err(|_| *server = None)?
        {
            RemoveResponse { error: None } => Ok(Some(Message::Deleted(id))),
            RemoveResponse { error: Some(e) } => Err(e.into()),
        },
        Command::Search { query, kind } => {
            shitty_refresh(database);

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
            {
                send_paste_buffer(
                    maybe_init_server(paste_socket_file, connect_to_paste_server, paste_server)?,
                    entry,
                    reader,
                    true,
                )
            }
            .inspect_err(|_| *paste_server = None)?;
            Ok(Some(Message::Pasted))
        }
    }
}

#[must_use]
pub fn ui_entry_(
    entry: Entry,
    loaded: &[u8],
    mime_type: &str,
    mut highlight: Option<(usize, usize)>,
) -> UiEntry {
    if mime_type.starts_with("image/") {
        return UiEntry {
            entry,
            cache: UiEntryCache::Image,
        };
    }

    let prefix_free = if let Some((start, end)) = &mut highlight {
        let mut l = &loaded[start.saturating_sub(24)..];
        for &b in l.iter().take(3) {
            // https://github.com/rust-lang/rust/blob/33422e72c8a66bdb5ee21246a948a1a02ca91674/library/core/src/num/mod.rs#L1090
            #[allow(clippy::cast_possible_wrap)]
            let is_utf8_char_boundary = (b as i8) >= -0x40;
            if is_utf8_char_boundary {
                break;
            }
            l = &l[1..];
        }

        let diff = loaded.len() - l.len();
        *start -= diff;
        *end -= diff;

        l
    } else {
        loaded
    };
    let suffix_free = &prefix_free[..min(prefix_free.len(), 250)];

    match str::from_utf8(suffix_free) {
        Ok(s) => Some(s),
        Err(e) if e.error_len().is_none() => {
            Some(unsafe { str::from_utf8_unchecked(&suffix_free[..e.valid_up_to()]) })
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
            let mut one_liner = String::with_capacity(s.len());

            if prefix_free.len() != loaded.len() {
                one_liner.push('…');
                if let Some((start, end)) = &mut highlight {
                    let prefix = const { '…'.len_utf8() };
                    *start += prefix;
                    *end += prefix;
                }
            }
            let mut prev_char_is_whitespace = false;
            for c in s.chars() {
                let mut offset_highlight = |diff| {
                    if let Some((start, end)) = &mut highlight {
                        if one_liner.len() < *start {
                            *start -= diff;
                        }
                        if one_liner.len() < *end {
                            *end -= diff;
                        }
                    }
                };

                if (prev_char_is_whitespace || one_liner.is_empty()) && c.is_whitespace() {
                    offset_highlight(c.len_utf8());
                    continue;
                }

                one_liner.push(if c.is_whitespace() {
                    offset_highlight(c.len_utf8() - const { ' '.len_utf8() });
                    ' '
                } else {
                    c
                });
                prev_char_is_whitespace = c.is_whitespace();
            }
            if suffix_free.len() != prefix_free.len() {
                one_liner.push('…');
            }
            if let Some((_, end)) = &mut highlight {
                *end = min(*end, one_liner.len());
            }

            UiEntry {
                entry,
                cache: if let Some((start, end)) = highlight {
                    UiEntryCache::HighlightedText {
                        one_liner: one_liner.into(),
                        start,
                        end,
                    }
                } else {
                    UiEntryCache::Text {
                        one_liner: one_liner.into(),
                    }
                },
            }
        },
    )
}

fn ui_entry(entry: Entry, reader: &mut EntryReader, highlight: Option<(usize, usize)>) -> UiEntry {
    let mut run = || {
        let loaded = entry.to_slice(reader)?;
        let mime_type = &*loaded.mime_type()?;
        Ok(ui_entry_(entry, &loaded, mime_type, highlight))
    };

    run().unwrap_or_else(|e| UiEntry {
        cache: UiEntryCache::Error(e),
        entry,
    })
}

type SearchCache = (
    Option<(u32, u32)>,
    HashMap<BucketAndIndex, RingAndIndex, BuildHasherDefault<FxHasher>>,
    Vec<SearchEntry>,
);

#[derive(Debug)]
struct SearchEntry {
    rai: RingAndIndex,
    start: usize,
    end: usize,
}

impl Eq for SearchEntry {}

impl PartialEq<Self> for SearchEntry {
    fn eq(&self, other: &Self) -> bool {
        self.rai.eq(&other.rai)
    }
}

impl PartialOrd<Self> for SearchEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SearchEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rai.cmp(&other.rai)
    }
}

fn do_search<E>(
    query: Query,
    reader_: &mut Option<EntryReader>,
    database: &mut DatabaseReader,
    mut send: impl FnMut(Message) -> Result<(), E>,
    (cached_write_heads, reverse_index_cache, search_result_buf): &mut SearchCache,
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
        reverse_index_cache.shrink_to_fit();
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
    for entry in result_stream.flatten().flat_map(
        |QueryResult {
             location,
             start,
             end,
         }|
         -> Result<_, CoreError> {
            let entry = match location {
                EntryLocation::Bucketed { bucket, index } => reverse_index_cache
                    .get(&BucketAndIndex::new(bucket, index))
                    .copied()
                    .ok_or_else(|| {
                        CoreError::IdNotFound(IdNotFoundError::Entry(
                            (index << u8::BITS) | u32::from(bucket),
                        ))
                    }),
                EntryLocation::File { entry_id } => {
                    RingAndIndex::from_id(entry_id).map_err(CoreError::IdNotFound)
                }
            }?;
            Ok(SearchEntry {
                rai: RingAndIndex::new(
                    entry.ring(),
                    write_heads[entry.ring() as usize].wrapping_sub(entry.index()) & MAX_ENTRIES,
                ),
                start,
                end,
            })
        },
    ) {
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
        .flat_map(|SearchEntry { rai, start, end }| -> Result<_, CoreError> {
            let entry = {
                let ring = rai.ring();
                let index = write_heads[ring as usize].wrapping_sub(rai.index()) & MAX_ENTRIES;

                let id = composite_id(ring, index);
                unsafe { database.get(id) }?
            };

            Ok(ui_entry(
                entry,
                reader,
                if start == end {
                    None
                } else {
                    Some((start, end))
                },
            ))
        })
        .collect();
    *search_result_buf = results;
    entries
}
