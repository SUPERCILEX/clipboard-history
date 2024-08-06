use std::{
    ffi::CStr,
    io,
    io::ErrorKind,
    mem::MaybeUninit,
    os::fd::OwnedFd,
    str,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
        mpsc::{SendError, SyncSender},
        Arc,
    },
    thread,
    thread::JoinHandle,
};

use arrayvec::ArrayVec;
use memchr::memmem::Finder;
use regex::bytes::Regex;
use ringboard_core::{
    bucket_to_length, ring::Mmap, size_to_bucket, IoErr, DIRECT_FILE_NAME_LEN, TEXT_MIMES,
};
use rustix::{
    fs::{openat, Mode, OFlags, RawDir},
    thread::{unshare, UnshareFlags},
};
use thiserror::Error;

use crate::{ring_reader::xattr_mime_type, EntryReader};

#[derive(Clone, Debug)]
pub struct CaselessQuery {
    query: Vec<u8>,
    trim: bool,
}

impl CaselessQuery {
    pub fn new<Q: Into<Vec<u8>>>(query: Q) -> Self {
        Self {
            query: query.into(),
            trim: false,
        }
    }

    #[must_use]
    pub const fn trim(mut self) -> Self {
        self.trim = true;
        self
    }
}

#[derive(Clone, Debug)]
pub enum Query<'a> {
    Plain(&'a [u8]),
    PlainIgnoreCase(CaselessQuery),
    Regex(Regex),
    Mimes(Regex),
}

trait QueryImpl {
    fn find(&mut self, haystack: &[u8]) -> Option<(usize, usize)>;

    fn needle_len(&self) -> Option<usize>;
}

#[derive(Clone)]
struct PlainQuery(Arc<Finder<'static>>);

impl QueryImpl for PlainQuery {
    fn find(&mut self, haystack: &[u8]) -> Option<(usize, usize)> {
        self.0
            .find(haystack)
            .map(|start| (start, start + self.0.needle().len()))
    }

    fn needle_len(&self) -> Option<usize> {
        Some(self.0.needle().len())
    }
}

#[derive(Clone)]
struct PlainIgnoreCaseQuery {
    inner: PlainQuery,
    cache: Vec<u8>,
}

impl QueryImpl for PlainIgnoreCaseQuery {
    fn find(&mut self, haystack: &[u8]) -> Option<(usize, usize)> {
        self.cache.clear();
        self.cache
            .extend(haystack.iter().map(u8::to_ascii_lowercase));

        self.inner.find(&self.cache)
    }

    fn needle_len(&self) -> Option<usize> {
        self.inner.needle_len()
    }
}

#[derive(Clone)]
struct RegexQuery(Regex);

impl QueryImpl for RegexQuery {
    fn find(&mut self, haystack: &[u8]) -> Option<(usize, usize)> {
        self.0.find(haystack).map(|m| (m.start(), m.end()))
    }

    fn needle_len(&self) -> Option<usize> {
        None
    }
}

#[derive(Copy, Clone, Debug)]
pub struct QueryResult {
    pub location: EntryLocation,
    pub start: usize,
    pub end: usize,
}

#[derive(Copy, Clone, Debug)]
pub enum EntryLocation {
    Bucketed { bucket: u8, index: u32 },
    File { entry_id: u64 },
}

#[derive(Clone, Debug)]
pub struct CancellationToken {
    stop: Arc<AtomicBool>,
}

impl CancellationToken {
    fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

pub struct QueryIter {
    stream: mpsc::IntoIter<Result<QueryResult, ringboard_core::Error>>,
    token: CancellationToken,
}

impl QueryIter {
    #[must_use]
    pub const fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Iterator for QueryIter {
    type Item = Result<QueryResult, ringboard_core::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.stream.next()
    }
}

impl Drop for QueryIter {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

pub fn search(
    query: Query,
    reader: Arc<EntryReader>,
) -> (
    QueryIter,
    impl Iterator<Item = JoinHandle<()>> + Send + Sync + 'static,
) {
    let (results, threads) = match query {
        Query::Plain(p) => search_impl(PlainQuery(Arc::new(Finder::new(p).into_owned())), reader),
        Query::PlainIgnoreCase(CaselessQuery { mut query, trim }) => {
            query.make_ascii_lowercase();
            let query = if trim { query.trim_ascii() } else { &query };
            search_impl(
                PlainIgnoreCaseQuery {
                    inner: PlainQuery(Arc::new(Finder::new(query).into_owned())),
                    cache: Vec::new(),
                },
                reader,
            )
        }
        Query::Regex(r) => search_impl(RegexQuery(r), reader),
        Query::Mimes(r) => mime_search_impl(RegexQuery(r), reader),
    };
    (results, threads.into_iter())
}

fn search_impl(
    mut query: impl QueryImpl + Clone + Send + 'static,
    reader: Arc<EntryReader>,
) -> (QueryIter, arrayvec::IntoIter<JoinHandle<()>, 13>) {
    let (sender, receiver) = mpsc::sync_channel(0);
    let token = CancellationToken::new();
    let mut threads = ArrayVec::<_, 13>::new_const();

    for bucket in usize::from(size_to_bucket(
        u16::try_from(query.needle_len().unwrap_or(0)).unwrap_or(u16::MAX),
    ))..reader.buckets().len()
    {
        let mut query = query.clone();
        let reader = reader.clone();
        let sender = sender.clone();
        let token = token.clone();
        threads.push(thread::spawn(move || {
            for (index, entry) in reader.buckets()[bucket]
                .chunks_exact(usize::from(bucket_to_length(bucket)))
                .enumerate()
            {
                if token.is_cancelled() {
                    break;
                }

                let entry = memchr::memchr(0, entry).map_or(entry, |stop| &entry[..stop]);
                let Some((start, end)) = query.find(entry) else {
                    continue;
                };
                if sender
                    .send(Ok(QueryResult {
                        location: EntryLocation::Bucketed {
                            bucket: u8::try_from(bucket).unwrap(),
                            index: u32::try_from(index).unwrap(),
                        },
                        start,
                        end,
                    }))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    {
        let (direct_file_sender, direct_file_receiver) = mpsc::sync_channel(8);
        threads.push(thread::spawn({
            let token = token.clone();
            let sender = sender.clone();
            move || {
                stream_through_direct_allocations(
                    &reader,
                    &token,
                    &sender,
                    |file_name, fd, mime_type| {
                        if !is_searchable_mime(mime_type) {
                            return Ok(());
                        }

                        let data = Mmap::from(&fd).map_io_err(|| {
                            format!("Failed to mmap direct allocation: {file_name:?}")
                        })?;
                        let file_name = <[u8; DIRECT_FILE_NAME_LEN]>::try_from(
                            file_name.to_bytes(),
                        )
                        .map_err(|_| ringboard_core::Error::Io {
                            error: io::Error::new(
                                ErrorKind::InvalidData,
                                "Not a Ringboard database.",
                            ),
                            context: format!(
                                "Direct allocation file name is of invalid size: {file_name:?}"
                            )
                            .into(),
                        })?;
                        direct_file_sender.send((data, file_name))?;
                        Ok(())
                    },
                );
            }
        }));
        threads.push(thread::spawn({
            let token = token.clone();
            move || {
                for (file, file_name) in direct_file_receiver {
                    if token.is_cancelled() {
                        break;
                    }

                    let mut run = || {
                        // TODO consider splitting this off into its own thread if big enough
                        let Some((start, end)) = query.find(&file) else {
                            return Ok(None);
                        };

                        let id = entry_id_from_direct_file_name(&file_name)?;
                        Ok(Some(QueryResult {
                            location: EntryLocation::File { entry_id: id },
                            start,
                            end,
                        }))
                    };

                    if match run() {
                        Ok(Some(r)) => sender.send(Ok(r)),
                        Ok(None) => continue,
                        Err(e) => sender.send(Err(e)),
                    }
                    .is_err()
                    {
                        break;
                    }
                }
            }
        }));
    }

    (
        QueryIter {
            stream: receiver.into_iter(),
            token,
        },
        threads.into_iter(),
    )
}

#[derive(Error, Debug)]
enum DirectIterError<T> {
    Core(#[from] ringboard_core::Error),
    Send(#[from] SendError<T>),
}

fn stream_through_direct_allocations<T, U>(
    reader: &EntryReader,
    token: &CancellationToken,
    sender: &SyncSender<Result<T, ringboard_core::Error>>,
    mut f: impl FnMut(&CStr, OwnedFd, &str) -> Result<(), DirectIterError<U>>,
) {
    let direct_dir = match openat(reader.direct(), c".", OFlags::DIRECTORY, Mode::empty())
        .map_io_err(|| "Failed to open direct dir.")
        .and_then(|fd| {
            unshare(UnshareFlags::FILES).map_io_err(|| "Failed to unshare FD table.")?;
            Ok(fd)
        }) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = sender.send(Err(e));
            return;
        }
    };

    let mut buf = [MaybeUninit::uninit(); 8192];
    let mut iter = RawDir::new(&direct_dir, &mut buf);
    while let Some(file) = iter.next() {
        if token.is_cancelled() {
            break;
        }

        let run = || {
            let file = file.map_io_err(|| "Failed to read direct allocation directory.")?;

            let file_name = file.file_name();
            if file_name == c"." || file_name == c".." {
                return Ok(());
            }

            let fd = openat(&direct_dir, file_name, OFlags::RDONLY, Mode::empty())
                .map_io_err(|| format!("Failed to open direct allocation: {file_name:?}"))?;
            let mime_type = xattr_mime_type(&fd)?;
            f(file_name, fd, &mime_type)
        };

        match run() {
            Ok(()) => continue,
            Err(DirectIterError::Core(e)) => {
                if sender.send(Err(e)).is_err() {
                    break;
                }
            }
            Err(DirectIterError::Send(_)) => break,
        }
    }
}

fn entry_id_from_direct_file_name(file_name: &[u8]) -> Result<u64, ringboard_core::Error> {
    str::from_utf8(file_name)
        .ok()
        .and_then(|id| u64::from_str(id).ok())
        .ok_or_else(|| ringboard_core::Error::Io {
            error: io::Error::new(ErrorKind::InvalidData, "Not a Ringboard database."),
            context: format!(
                "Invalid direct allocation file name: {:?}",
                file_name.escape_ascii()
            )
            .into(),
        })
}

fn is_searchable_mime(mime: &str) -> bool {
    TEXT_MIMES.contains(&mime) || mime.starts_with("text/") || mime == "application/xml"
}

fn mime_search_impl(
    mut query: impl QueryImpl + Clone + Send + 'static,
    reader: Arc<EntryReader>,
) -> (QueryIter, arrayvec::IntoIter<JoinHandle<()>, 13>) {
    let (sender, receiver) = mpsc::sync_channel(0);
    let token = CancellationToken::new();
    let mut threads = ArrayVec::<_, 13>::new_const();

    threads.push(thread::spawn({
        let token = token.clone();
        move || {
            stream_through_direct_allocations(
                &reader,
                &token,
                &sender,
                |file_name, _fd, mime_type| {
                    if mime_type.is_empty() {
                        return Ok(());
                    }

                    if query.find(mime_type.as_bytes()).is_some() {
                        let id = entry_id_from_direct_file_name(file_name.to_bytes())?;
                        sender.send(Ok(QueryResult {
                            location: EntryLocation::File { entry_id: id },
                            start: 0,
                            end: 0,
                        }))?;
                    }
                    Ok(())
                },
            );
        }
    }));

    (
        QueryIter {
            stream: receiver.into_iter(),
            token,
        },
        threads.into_iter(),
    )
}
