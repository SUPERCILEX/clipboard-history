use std::{
    ffi::CStr,
    io,
    io::ErrorKind,
    mem::MaybeUninit,
    os::fd::OwnedFd,
    str,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    thread::JoinHandle,
};

use arrayvec::ArrayVec;
use crossbeam_channel::{RecvError, SendError, Sender};
use memchr::memmem::Finder;
use regex::bytes::Regex;
use ringboard_core::{
    DIRECT_FILE_NAME_LEN, Error as CoreError, IoErr, bucket_to_length, ring::Mmap, size_to_bucket,
};
use rustix::{
    fs::{Mode, OFlags, RawDir, openat},
    thread::{UnshareFlags, unshare},
};

use crate::{
    EntryReader,
    ring_reader::{is_text_mime, xattr_mime_type},
};

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

impl Clone for PlainIgnoreCaseQuery {
    fn clone(&self) -> Self {
        let Self { inner, cache: _ } = self;
        Self {
            inner: inner.clone(),
            cache: Vec::new(),
        }
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

type Out = Result<QueryResult, CoreError>;

pub struct QueryIter<const N: usize> {
    receiver: crossbeam_channel::Receiver<ArrayVec<Out, N>>,
    buf: ArrayVec<Out, N>,
    token: CancellationToken,
}

impl<const N: usize> QueryIter<N> {
    #[must_use]
    pub const fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }
}

impl<const N: usize> Iterator for QueryIter<N> {
    type Item = Out;

    fn next(&mut self) -> Option<Self::Item> {
        let Self {
            receiver,
            buf,
            token: _,
        } = self;
        loop {
            if let Some(v) = buf.pop() {
                return Some(v);
            }
            match receiver.recv() {
                Ok(new) => *buf = new,
                Err(RecvError) => return None,
            }
        }
    }
}

impl<const N: usize> Drop for QueryIter<N> {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

pub fn search(
    query: Query,
    reader: Arc<EntryReader>,
) -> (
    QueryIter<16>,
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

struct BufferedSender<const N: usize, T> {
    buf: ArrayVec<T, N>,
    sender: Sender<ArrayVec<T, N>>,
}

impl<const N: usize, T> BufferedSender<N, T> {
    const fn new(sender: Sender<ArrayVec<T, N>>) -> Self {
        Self {
            buf: ArrayVec::new_const(),
            sender,
        }
    }

    fn send(&mut self, value: T) -> Result<(), SendError<ArrayVec<T, N>>> {
        let Self { buf, sender } = self;
        buf.push(value);
        if buf.is_full() {
            sender.send(buf.take())
        } else {
            Ok(())
        }
    }
}

impl<const N: usize, T> Drop for BufferedSender<N, T> {
    fn drop(&mut self) {
        let Self { buf, sender } = self;
        if !buf.is_empty() {
            let _ = sender.send(buf.take());
        }
    }
}

fn search_impl<const N: usize>(
    mut query: impl QueryImpl + Clone + Send + 'static,
    reader: Arc<EntryReader>,
) -> (QueryIter<N>, arrayvec::IntoIter<JoinHandle<()>, 13>) {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    let token = CancellationToken::new();
    let mut threads = ArrayVec::<_, 13>::new_const();

    let mut extra_direct_threads = 1;
    let (direct_file_sender, direct_file_receiver) = crossbeam_channel::bounded(1);
    for bucket in usize::from(size_to_bucket(
        u16::try_from(query.needle_len().unwrap_or(0)).unwrap_or(u16::MAX),
    ))..reader.buckets().len()
    {
        let mut query = query.clone();
        let reader = reader.clone();
        let sender = sender.clone();
        let token = token.clone();
        let direct_file_receiver = if extra_direct_threads > 0 {
            extra_direct_threads -= 1;
            Some(direct_file_receiver.clone())
        } else {
            None
        };
        threads.push(thread::spawn(move || {
            let mut sender = BufferedSender::new(sender);
            {
                let bucket_size = usize::from(bucket_to_length(bucket));
                let midpoint = if bucket_size == 4 {
                    1
                } else {
                    bucket_size / 2 + 1
                };
                for (index, entry) in reader.buckets()[bucket]
                    .chunks_exact(bucket_size)
                    .enumerate()
                {
                    if token.is_cancelled() {
                        break;
                    }

                    let entry = memchr::memchr(0, &entry[midpoint..])
                        .map_or(entry, |stop| &entry[..midpoint + stop]);
                    let Some((start, end)) = query.find(entry) else {
                        continue;
                    };
                    let _ = sender.send(Ok(QueryResult {
                        location: EntryLocation::Bucketed {
                            bucket: u8::try_from(bucket).unwrap(),
                            index: u32::try_from(index).unwrap(),
                        },
                        start,
                        end,
                    }));
                }
            }
            if let Some(directs) = direct_file_receiver {
                direct_alloc_search_stream(&token, &mut query, directs, |r| sender.send(r));
            }
        }));
    }
    threads.push(thread::spawn({
        let token = token.clone();
        let sender = sender.clone();
        move || {
            let mut sender = BufferedSender::new(sender);
            stream_through_direct_allocations(
                &reader,
                &token,
                &mut sender,
                |_, file_name, fd, mime_type| {
                    if !is_text_mime(mime_type) {
                        return Ok(());
                    }

                    let data = Mmap::from(&fd).map_io_err(|| {
                        format!("Failed to mmap direct allocation: {file_name:?}")
                    })?;
                    let file_name = <[u8; DIRECT_FILE_NAME_LEN]>::try_from(file_name.to_bytes())
                        .map_err(|_| CoreError::Io {
                            error: io::Error::new(
                                ErrorKind::InvalidData,
                                "Not a Ringboard database.",
                            ),
                            context: format!(
                                "Direct allocation file name is of invalid size: {file_name:?}"
                            )
                            .into(),
                        })?;
                    let _ = direct_file_sender.send((data, file_name));
                    Ok(())
                },
            );
        }
    }));
    threads.push(thread::spawn({
        let token = token.clone();
        move || {
            let mut sender = BufferedSender::new(sender);
            direct_alloc_search_stream(&token, &mut query, direct_file_receiver, |r| {
                sender.send(r)
            });
        }
    }));

    (
        QueryIter {
            receiver,
            buf: ArrayVec::new_const(),
            token,
        },
        threads.into_iter(),
    )
}

fn direct_alloc_search_stream<U>(
    token: &CancellationToken,
    query: &mut impl QueryImpl,
    inputs: impl IntoIterator<Item = (Mmap, [u8; DIRECT_FILE_NAME_LEN])>,
    mut send: impl FnMut(Out) -> Result<(), U>,
) {
    for (file, file_name) in inputs {
        if token.is_cancelled() {
            break;
        }

        let mut run = || {
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
            Ok(Some(r)) => send(Ok(r)),
            Ok(None) => Ok(()),
            Err(e) => send(Err(e)),
        }
        .is_err()
        {
            break;
        }
    }
}

fn stream_through_direct_allocations<const N: usize>(
    reader: &EntryReader,
    token: &CancellationToken,
    sender: &mut BufferedSender<N, Out>,
    mut f: impl FnMut(&mut BufferedSender<N, Out>, &CStr, OwnedFd, &str) -> Result<(), CoreError>,
) {
    let (direct_dir, metadata_dir) = {
        let run = || {
            let direct_dir = openat(reader.direct(), c".", OFlags::DIRECTORY, Mode::empty())
                .map_io_err(|| "Failed to open direct dir.")?;
            let metadata_dir = if let Some(metadata_dir) = reader.metadata() {
                Some(
                    openat(metadata_dir, c".", OFlags::DIRECTORY, Mode::empty())
                        .map_io_err(|| "Failed to open metadata dir.")?,
                )
            } else {
                None
            };

            unshare(UnshareFlags::FILES).map_io_err(|| "Failed to unshare FD table.")?;

            Ok((direct_dir, metadata_dir))
        };

        match run() {
            Ok(d) => d,
            Err(e) => {
                let _ = sender.send(Err(e));
                return;
            }
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
            let mime_type = xattr_mime_type(&fd, metadata_dir.as_ref().map(|d| (d, file_name)))?;
            f(sender, file_name, fd, &mime_type)
        };

        match run() {
            Ok(()) => (),
            Err(e) => {
                let _ = sender.send(Err(e));
            }
        }
    }
}

fn entry_id_from_direct_file_name(file_name: &[u8]) -> Result<u64, CoreError> {
    str::from_utf8(file_name)
        .ok()
        .and_then(|id| u64::from_str(id).ok())
        .ok_or_else(|| CoreError::Io {
            error: io::Error::new(ErrorKind::InvalidData, "Not a Ringboard database."),
            context: format!(
                "Invalid direct allocation file name: {}",
                file_name.escape_ascii()
            )
            .into(),
        })
}

fn mime_search_impl<const N: usize>(
    mut query: impl QueryImpl + Clone + Send + 'static,
    reader: Arc<EntryReader>,
) -> (QueryIter<N>, arrayvec::IntoIter<JoinHandle<()>, 13>) {
    let (sender, receiver) = crossbeam_channel::bounded(0);
    let token = CancellationToken::new();
    let mut threads = ArrayVec::<_, 13>::new_const();

    threads.push(thread::spawn({
        let token = token.clone();
        move || {
            let mut sender = BufferedSender::new(sender);
            stream_through_direct_allocations(
                &reader,
                &token,
                &mut sender,
                |sender, file_name, _, mime_type| {
                    if mime_type.is_empty() {
                        return Ok(());
                    }

                    if query.find(mime_type.as_bytes()).is_some() {
                        let id = entry_id_from_direct_file_name(file_name.to_bytes())?;
                        let _ = sender.send(Ok(QueryResult {
                            location: EntryLocation::File { entry_id: id },
                            start: 0,
                            end: 0,
                        }));
                    }
                    Ok(())
                },
            );
        }
    }));

    (
        QueryIter {
            receiver,
            buf: ArrayVec::new_const(),
            token,
        },
        threads.into_iter(),
    )
}
