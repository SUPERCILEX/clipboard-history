use std::{
    array,
    cmp::min,
    collections::{BinaryHeap, HashMap},
    hash::BuildHasherDefault,
    io::BufReader,
    iter::once,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    str,
    sync::Arc,
};

use image::{DynamicImage, ImageError, ImageReader};
use regex::bytes::Regex;
use rustc_hash::FxHasher;
use rustix::{
    fs::{openat, statx, AtFlags, Mode, OFlags, StatxFlags, CWD},
    net::SocketAddrUnix,
};
use thiserror::Error;

use crate::{
    api::{connect_to_server, MoveToFrontRequest, RemoveRequest},
    core::{
        dirs::{data_dir, socket_file},
        protocol::{composite_id, IdNotFoundError, MoveToFrontResponse, RemoveResponse, RingKind},
        ring::{offset_to_entries, Ring, MAX_ENTRIES},
        size_to_bucket, BucketAndIndex, Error as CoreError, IoErr, PathView, RingAndIndex,
    },
    search,
    search::{EntryLocation, Query},
    ClientError, DatabaseReader, Entry, EntryReader, Kind,
};

#[derive(Error, Debug)]
pub enum CommandError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Sdk(#[from] ClientError),
    #[error("Regex instantiation failed.")]
    Regex(#[from] regex::Error),
    #[error("Image loading error.")]
    Image(#[from] ImageError),
}

impl From<IdNotFoundError> for CommandError {
    fn from(value: IdNotFoundError) -> Self {
        Self::Core(CoreError::IdNotFound(value))
    }
}

#[derive(Debug)]
pub enum Command {
    RefreshDb,
    LoadFirstPage,
    GetDetails { entry: Entry, with_text: bool },
    Favorite(u64),
    Unfavorite(u64),
    Delete(u64),
    Search { query: Box<str>, regex: bool },
    LoadImage(u64),
}

#[derive(Debug)]
pub enum Message {
    FatalDbOpen(CoreError),
    FatalServerConnect(ClientError),
    Error(CommandError),
    LoadedFirstPage {
        entries: Box<[UiEntry]>,
        default_focused_id: Option<u64>,
    },
    EntryDetails {
        id: u64,
        result: Result<DetailedEntry, CoreError>,
    },
    SearchResults(Box<[UiEntry]>),
    FavoriteChange(u64),
    LoadedImage {
        id: u64,
        image: DynamicImage,
    },
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
    Image,
    Binary {
        mime_type: Box<str>,
        context: Box<str>,
    },
    Error(CoreError),
}

#[derive(Debug)]
pub struct DetailedEntry {
    pub mime_type: Box<str>,
    pub full_text: Option<Box<str>>,
}

pub fn controller<T>(
    commands: impl IntoIterator<Item = Command>,
    mut send: impl FnMut(Message) -> Result<(), T>,
) {
    fn maybe_init_server(
        cache: &mut Option<(OwnedFd, SocketAddrUnix)>,
    ) -> Result<(impl AsFd + '_, &SocketAddrUnix), ClientError> {
        if cache.is_some() {
            let (sock, addr) = cache.as_ref().unwrap();
            Ok((sock, addr))
        } else {
            match {
                let socket_file = socket_file();
                SocketAddrUnix::new(&socket_file)
                    .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))
            }
            .map_err(ClientError::from)
            .and_then(|server_addr| Ok((connect_to_server(&server_addr)?, server_addr)))
            {
                Ok(s) => {
                    let (sock, addr) = cache.get_or_insert(s);
                    Ok((sock, addr))
                }
                Err(e) => Err(e),
            }
        }
    }

    let mut server = None;
    let ((mut database, reader), rings) = {
        let run = || {
            let mut dir = data_dir();

            let database = DatabaseReader::open(&mut dir)?;
            let reader = EntryReader::open(&mut dir)?;

            let mut open_ring = |kind: RingKind| {
                let path = PathView::new(&mut dir, kind.file_name());
                openat(CWD, &*path, OFlags::PATH, Mode::empty()).map_io_err(|| {
                    format!("Failed to open Ringboard database for reading: {path:?}")
                })
            };
            let rings = (open_ring(RingKind::Main)?, open_ring(RingKind::Favorites)?);

            Ok(((database, reader), rings))
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
    let mut reverse_index_cache = HashMap::default();

    for command in once(Command::LoadFirstPage).chain(commands) {
        let result = handle_command(
            command,
            || maybe_init_server(&mut server),
            &mut database,
            &mut reader,
            &(&rings.0, &rings.1),
            &mut reverse_index_cache,
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

#[allow(clippy::too_many_lines)]
fn handle_command<'a, Server: AsFd>(
    command: Command,
    server: impl FnOnce() -> Result<(Server, &'a SocketAddrUnix), ClientError>,
    database: &mut DatabaseReader,
    reader_: &mut Option<EntryReader>,
    rings: &(impl AsFd, impl AsFd),
    reverse_index_cache: &mut HashMap<BucketAndIndex, RingAndIndex, BuildHasherDefault<FxHasher>>,
) -> Result<Option<Message>, CommandError> {
    let reader = reader_.as_mut().unwrap();
    match command {
        Command::RefreshDb => {
            reverse_index_cache.clear();
            let run = |ring: &mut Ring, fd: BorrowedFd| {
                let len = statx(fd, c"", AtFlags::EMPTY_PATH, StatxFlags::SIZE)
                    .map_io_err(|| "Failed to statx Ringboard database file.")?
                    .stx_size;
                let len = offset_to_entries(usize::try_from(len).unwrap());
                unsafe {
                    ring.set_len(len);
                }
                Ok::<_, CoreError>(())
            };
            run(database.main_ring_mut(), rings.0.as_fd())?;
            run(database.favorites_ring_mut(), rings.1.as_fd())?;

            Ok(None)
        }
        Command::LoadFirstPage => {
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
        Command::GetDetails { entry, with_text } => {
            let mut run = || {
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
            Ok(Some(Message::EntryDetails {
                id: entry.id(),
                result: run(),
            }))
        }
        ref c @ (Command::Favorite(id) | Command::Unfavorite(id)) => {
            let (server, addr) = server()?;
            match MoveToFrontRequest::response(
                server,
                addr,
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
        Command::Delete(id) => {
            let (server, addr) = server()?;
            match RemoveRequest::response(server, addr, id)? {
                RemoveResponse { error: Some(e) } => return Err(e.into()),
                RemoveResponse { error: None } => {}
            }
            Ok(None)
        }
        Command::Search { mut query, regex } => {
            let query = if regex {
                Query::Regex(Regex::new(&query)?)
            } else if query
                .chars()
                .all(|c| !char::is_alphabetic(c) || char::is_lowercase(c))
            {
                query.make_ascii_lowercase();
                Query::PlainIgnoreCase(query.trim().as_bytes())
            } else {
                Query::Plain(query.trim().as_bytes())
            };
            Ok(Some(Message::SearchResults(
                do_search(query, reader_, database, reverse_index_cache).into(),
            )))
        }
        Command::LoadImage(id) => {
            let entry = unsafe { database.get(id)? };
            Ok(Some(Message::LoadedImage {
                id,
                image: ImageReader::new(BufReader::new(&*entry.to_file(reader)?))
                    .with_guessed_format()
                    .map_io_err(|| "Failed to guess image format for entry {id}.")?
                    .decode()?,
            }))
        }
    }
}

fn ui_entry(entry: Entry, reader: &mut EntryReader) -> Result<UiEntry, CoreError> {
    let loaded = entry.to_slice(reader)?;
    let mime_type = &*loaded.mime_type()?;
    let entry = if mime_type.starts_with("image/") {
        UiEntry {
            entry,
            cache: UiEntryCache::Image,
        }
    } else if let Ok(s) = {
        let mut shrunk = &loaded[..min(loaded.len(), 250)];
        loop {
            let Some(&b) = shrunk.last() else {
                break;
            };
            // https://github.com/rust-lang/rust/blob/33422e72c8a66bdb5ee21246a948a1a02ca91674/library/core/src/num/mod.rs#L1090
            #[allow(clippy::cast_possible_wrap)]
            let is_utf8_char_boundary = (b as i8) >= -0x40;
            if is_utf8_char_boundary || loaded.len() == shrunk.len() {
                break;
            }

            shrunk = &loaded[..=shrunk.len()];
        }
        str::from_utf8(shrunk)
    } {
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
    } else {
        UiEntry {
            entry,
            cache: UiEntryCache::Binary {
                mime_type: mime_type.into(),
                context: Box::default(),
            },
        }
    };
    Ok(entry)
}

fn do_search(
    query: Query,
    reader_: &mut Option<EntryReader>,
    database: &mut DatabaseReader,
    reverse_index_cache: &mut HashMap<BucketAndIndex, RingAndIndex, BuildHasherDefault<FxHasher>>,
) -> Vec<UiEntry> {
    const MAX_SEARCH_ENTRIES: usize = 256;

    let reader = Arc::new(reader_.take().unwrap());

    let (result_stream, threads) = search(query, reader.clone());

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

    let mut results = BinaryHeap::new();
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

    results
        .into_sorted_vec()
        .into_iter()
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
        .collect()
}
