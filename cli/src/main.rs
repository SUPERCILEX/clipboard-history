#![feature(debug_closure_helpers)]

use std::{
    borrow::Cow,
    cmp::{max, min},
    collections::{BTreeMap, HashMap, VecDeque},
    fmt::{Debug, Display, Formatter},
    fs,
    fs::{File, create_dir_all},
    hash::BuildHasherDefault,
    io,
    io::{BufReader, ErrorKind, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::FileExt,
    },
    path::{Path, PathBuf},
    str,
    sync::Arc,
};

use arrayvec::ArrayVec;
use ask::Answer;
use base64_serde::base64_serde_type;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, ValueHint};
use clap_num::si_number;
use error_stack::Report;
use rand::{
    Rng,
    distributions::{Alphanumeric, DistString, Standard},
};
use rand_distr::{Distribution, LogNormal, WeightedAliasIndex};
use rand_xoshiro::{
    Xoshiro256PlusPlus,
    rand_core::{RngCore, SeedableRng},
};
use regex::bytes::Regex;
use ringboard_sdk::{
    ClientError, DatabaseReader, EntryReader, Kind,
    api::{
        AddRequest, GarbageCollectRequest, MoveToFrontRequest, RemoveRequest, SwapRequest,
        connect_to_paste_server, connect_to_server, connect_to_server_with, send_paste_buffer,
    },
    config::{X11Config, X11V1Config, x11_config_file},
    core::{
        BucketAndIndex, Error as CoreError, IoErr, NUM_BUCKETS, SendQuitAndWait, acquire_lock_file,
        bucket_to_length, copy_file_range_all, create_tmp_file,
        dirs::{data_dir, paste_socket_file, socket_file},
        protocol::{
            AddResponse, GarbageCollectResponse, IdNotFoundError, MimeType, MoveToFrontResponse,
            RemoveResponse, Response, RingKind, SwapResponse, decompose_id,
        },
        read_at_to_end,
        ring::Mmap,
        size_to_bucket,
    },
    duplicate_detection::DuplicateDetector,
    search::{CaselessQuery, EntryLocation, Query, QueryResult},
};
use rustc_hash::FxHasher;
use rustix::{
    fs::{AtFlags, CWD, MemfdFlags, Mode, OFlags, StatxFlags, memfd_create, openat, statx},
    net::{RecvFlags, SendFlags, SocketAddrUnix, SocketFlags},
    stdio::stdin,
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeSeq};
use thiserror::Error;

/// The Ringboard (clipboard history) CLI.
///
/// Ringboard uses a client-server architecture, wherein the server has
/// exclusive write access to the clipboard database and clients must ask the
/// server to perform the modifications they need. This CLI is a non-interactive
/// client and a debugging tool.
#[derive(Parser, Debug)]
#[command(version, author = "Alex Saveau (@SUPERCILEX)")]
#[command(infer_subcommands = true, infer_long_args = true)]
#[command(disable_help_flag = true)]
#[command(arg_required_else_help = true)]
#[command(max_term_width = 100)]
#[cfg_attr(test, command(help_expected = true))]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    #[arg(short, long, short_alias = '?', global = true)]
    #[arg(action = ArgAction::Help, help = "Print help (use `--help` for more detail)")]
    #[arg(long_help = "Print help (use `-h` for a summary)")]
    help: Option<bool>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Get an entry from the database.
    ///
    /// The entry bytes will be outputted to stdout.
    #[command(aliases = ["g", "at", "gimme"])]
    Get(EntryAction),

    /// Searches the Ringboard database for entries matching a query.
    #[command(aliases = ["f", "find", "query"])]
    Search(Search),

    /// Add an entry to the database.
    ///
    /// Prints the ID of the newly added entry.
    #[command(aliases = ["a", "new", "create", "copy"])]
    Add(Add),

    /// Favorite an entry.
    #[command(alias = "star")]
    Favorite(EntryAction),

    /// Unfavorite an entry.
    #[command(alias = "unstar")]
    Unfavorite(EntryAction),

    /// Move an entry to the front, making it the most recent entry.
    MoveToFront(EntryAction),

    /// Swap the positions of two entries.
    ///
    /// One of the entries may be uninitialized. Thus, swap can be used to
    /// insert an entry into the ring by adding it and swapping the new entry
    /// into position.
    ///
    /// A set operation may also be implemented via swap by adding an entry,
    /// swapping it into place, and deleting the swapped out entry.
    Swap(Swap),

    /// Delete an entry from the database.
    #[command(aliases = ["r", "del", "delete", "destroy", "yeet"])]
    Remove(EntryAction),

    /// Wipe the entire database.
    ///
    /// WARNING: this operation is irreversible. ALL DATA WILL BE LOST.
    #[command(alias = "nuke")]
    Wipe,

    /// Migrate from other clipboard managers to Ringboard.
    #[command(alias = "migrate")]
    Import(Import),

    /// Run garbage collection on the database.
    ///
    /// Prints the amount of freed space.
    #[command(aliases = ["gc", "clean"])]
    GarbageCollect(GarbageCollect),

    /// Modify app settings.
    #[command(aliases = ["c", "config"])]
    #[command(subcommand)]
    Configure(Configure),

    /// Debugging tools for developers.
    #[command(aliases = ["d", "dev"])]
    #[command(subcommand)]
    Debug(Dev),
}

#[derive(Subcommand, Debug)]
enum Configure {
    /// Edit the X11 watcher settings.
    #[command(aliases = ["x"])]
    X11(ConfigureX11),
}

#[derive(Args, Debug)]
struct ConfigureX11 {
    /// Instead of simply placing selected items in the clipboard, attempt to
    /// automatically paste the selected item into the previously focused
    /// application.
    #[clap(long)]
    #[clap(default_value_t = true)]
    #[clap(action = ArgAction::Set)]
    auto_paste: bool,
}

#[derive(Subcommand, Debug)]
enum Dev {
    /// Print statistics about the Ringboard database.
    #[command(aliases = ["nerd", "kowalski-analysis"])]
    Stats,

    /// Dump the database contents for analysis.
    ///
    /// The JSON format is as follows:
    ///{n}[
    ///{n}  {
    ///{n}    "id": int64,
    ///{n}    "kind": "Human" | "Bytes",
    ///{n}    "data": (UTF-8 | base64) string
    ///{n}  },
    ///{n}  ...
    ///{n}]
    ///
    /// Note that `$ ringboard import json` expects a JSON stream (wherein each
    /// object appears on its own line instead of being in a list). To import an
    /// export, you can convert the JSON array to a stream with `$ ... | jq -c
    /// .[]`.
    #[command(alias = "export")]
    Dump,

    /// Generate a pseudo-random database for testing and performance tuning
    /// purposes.
    Generate(Generate),

    /// Spam the server with random commands.
    Fuzz(Fuzz),
}

#[derive(Args, Debug)]
struct Add {
    /// A file containing the data to be added to the entry.
    ///
    /// A value of `-` may be supplied to indicate that data should be read from
    /// STDIN.
    #[arg(value_hint = ValueHint::FilePath)]
    #[clap(default_value = "-")]
    data_file: PathBuf,

    /// Whether to add the entry to the favorites ring.
    #[clap(short, long)]
    #[clap(default_value_t = false)]
    favorite: bool,

    /// The entry mime type.
    #[clap(short, long, short_alias = 't', alias = "target")]
    mime_type: Option<MimeType>,

    /// Whether to overwrite the system clipboard with this entry.
    #[clap(short, long)]
    #[clap(default_value_t = false)]
    copy: bool,
}

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
struct EntryAction {
    /// The entry ID.
    #[arg(required = true)]
    id: u64,
}

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
struct Search {
    /// Interpret the query string as regex instead of a plain-text match.
    #[arg(short, long)]
    regex: bool,

    /// Ignore ASCII casing when searching.
    #[arg(short, long)]
    #[arg(conflicts_with = "regex")]
    ignore_case: bool,

    /// The query string to search for.
    #[arg(required = true)]
    query: String,
}

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
struct Swap {
    /// The first entry ID.
    #[arg(required = true)]
    id1: u64,

    /// The second entry ID.
    #[arg(required = true)]
    id2: u64,
}

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
struct Import {
    /// The existing clipboard to import.
    #[arg(required = true)]
    #[arg(requires_if("rb", "database"))]
    #[arg(requires_if("ring", "database"))]
    #[arg(requires_if("ringboard", "database"))]
    #[arg(requires_if("json", "database"))]
    from: ImportClipboard,

    /// The existing clipboard's database location.
    ///
    /// This will be automatically inferred by default.
    #[clap(value_hint = ValueHint::AnyPath)]
    database: Option<PathBuf>,
}

#[derive(ValueEnum, Copy, Clone, Debug)]
enum ImportClipboard {
    /// [Gnome Clipboard History](https://extensions.gnome.org/extension/4839/clipboard-history/)
    #[value(alias = "gch")]
    GnomeClipboardHistory,

    /// [Clipboard Indicator](https://extensions.gnome.org/extension/779/clipboard-indicator/)
    #[value(alias = "ci")]
    ClipboardIndicator,

    /// [GPaste](https://github.com/Keruspe/GPaste)
    #[value(aliases = ["gp", "gpaste"])]
    GPaste,

    /// A sequence of JSON objects in the same format as the dump command.
    // Make sure to update the Import::from requires_ifs when changing aliases
    #[value(aliases = ["rb", "ring", "ringboard"])]
    Json,
}

#[derive(Args, Debug)]
struct GarbageCollect {
    /// The maximum amount of garbage (in bytes) that is tolerable.
    ///
    /// A value of zero will perform maximal compaction including entry
    /// deduplication.
    #[arg(short, long)]
    #[arg(default_value_t = 0)]
    max_wasted_bytes: u64,
}

#[derive(Args, Debug)]
struct Generate {
    /// The number of random entries to generate.
    #[clap(short, long = "entries", alias = "num-entries")]
    #[clap(value_parser = si_number::< u32 >)]
    #[clap(default_value = "100_000")]
    num_entries: u32,

    /// The mean entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::< u32 >)]
    #[clap(default_value = "512")]
    mean_size: u32,

    /// The coefficient of variation of the entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::< u32 >)]
    #[clap(default_value = "10")]
    cv_size: u32,
}

#[derive(Args, Debug)]
struct Fuzz {
    /// The RNG seed.
    #[clap(short, long)]
    #[clap(default_value = "42")]
    seed: u64,

    /// The mean entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::< u32 >)]
    #[clap(default_value = "512")]
    mean_size: u32,

    /// The coefficient of variation of the entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::< u32 >)]
    #[clap(default_value = "10")]
    cv_size: u32,

    /// Print extra debugging output
    #[clap(short, long)]
    #[clap(default_value_t = false)]
    verbose: bool,
}

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Sdk(#[from] ClientError),
    #[error("failed to delete or copy files")]
    Fuc(#[from] fuc_engine::Error),
    #[error("database not found")]
    DatabaseNotFound(PathBuf),
    #[error("JSON (de)serialization failed")]
    SerdeJson(#[from] serde_json::Error),
    #[error("Quick XML (de)serialization failed")]
    QuickXml(#[from] quick_xml::Error),
    #[error("Serde XML (de)serialization failed")]
    QuickXmlDe(#[from] quick_xml::DeError),
    #[error("Serde XML (de)serialization failed")]
    QuickXmlAttr(#[from] quick_xml::events::attributes::AttrError),
    #[error("Serde TOML serialization failed")]
    Toml(#[from] toml::ser::Error),
    #[error("invalid RegEx")]
    Regex(#[from] regex::Error),
    #[error("internal search error")]
    InternalSearchError,
}

#[derive(Error, Debug)]
enum Wrapper {
    #[error("{0}")]
    W(String),
}

fn main() -> error_stack::Result<(), Wrapper> {
    #[cfg(not(debug_assertions))]
    error_stack::Report::install_debug_hook::<std::panic::Location>(|_, _| {});

    run().map_err(|e| {
        let wrapper = Wrapper::W(e.to_string());
        match e {
            CliError::Core(e) => e.into_report(wrapper),
            CliError::Fuc(fuc_engine::Error::Io { error, context }) => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            CliError::Sdk(e) => e.into_report(wrapper),
            CliError::DatabaseNotFound(db) => Report::new(wrapper)
                .attach_printable(
                    "Make sure to run the Ringboard server or fix the XDG_DATA_HOME path.",
                )
                .attach_printable(format!("Expected database directory: {:?}", db.display())),
            CliError::Fuc(e) => Report::new(e).change_context(wrapper),
            CliError::SerdeJson(e) => Report::new(e).change_context(wrapper),
            CliError::QuickXml(e) => Report::new(e).change_context(wrapper),
            CliError::QuickXmlDe(e) => Report::new(e).change_context(wrapper),
            CliError::QuickXmlAttr(e) => Report::new(e).change_context(wrapper),
            CliError::Toml(e) => Report::new(e).change_context(wrapper),
            CliError::Regex(e) => Report::new(e).change_context(wrapper),
            CliError::InternalSearchError => Report::new(wrapper).attach_printable(
                "Please report this bug at https://github.com/SUPERCILEX/clipboard-history/issues/new",
            ),
        }
    })
}

impl From<IdNotFoundError> for CliError {
    fn from(value: IdNotFoundError) -> Self {
        Self::Core(CoreError::IdNotFound(value))
    }
}

fn run() -> Result<(), CliError> {
    let Cli { cmd, help: _ } = Cli::parse();

    let server_addr = {
        let socket_file = socket_file();
        SocketAddrUnix::new(&socket_file)
            .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?
    };
    match cmd {
        Cmd::Get(data) => get(data),
        Cmd::Search(data) => search(data),
        Cmd::Add(data) => add(connect_to_server(&server_addr)?, data),
        Cmd::Favorite(data) => move_to_front(
            connect_to_server(&server_addr)?,
            data,
            Some(RingKind::Favorites),
        ),
        Cmd::Unfavorite(data) => {
            move_to_front(connect_to_server(&server_addr)?, data, Some(RingKind::Main))
        }
        Cmd::MoveToFront(data) => move_to_front(connect_to_server(&server_addr)?, data, None),
        Cmd::Swap(data) => swap(connect_to_server(&server_addr)?, data),
        Cmd::Remove(data) => remove(connect_to_server(&server_addr)?, data),
        Cmd::Wipe => wipe(),
        Cmd::GarbageCollect(data) => garbage_collect(connect_to_server(&server_addr)?, data),
        Cmd::Import(data) => import(connect_to_server(&server_addr)?, data),
        Cmd::Configure(Configure::X11(data)) => configure_x11(data),
        Cmd::Debug(Dev::Stats) => stats(),
        Cmd::Debug(Dev::Dump) => dump(),
        Cmd::Debug(Dev::Generate(data)) => generate(connect_to_server(&server_addr)?, data),
        Cmd::Debug(Dev::Fuzz(data)) => fuzz(&server_addr, data),
    }
}

fn open_db() -> Result<(DatabaseReader, EntryReader), CliError> {
    let mut database = data_dir();
    if !database
        .try_exists()
        .map_io_err(|| format!("Failed to check that database exists: {database:?}"))?
    {
        return Err(CliError::DatabaseNotFound(database));
    }

    Ok((
        DatabaseReader::open(&mut database)?,
        EntryReader::open(&mut database)?,
    ))
}

fn get(EntryAction { id }: EntryAction) -> Result<(), CliError> {
    let (database, mut reader) = open_db()?;
    let entry = database.get_raw(id)?;
    io::copy(&mut *entry.to_file(&mut reader)?, &mut io::stdout().lock())
        .map_io_err(|| "Failed to write entry to stdout")?;
    Ok(())
}

fn search(
    Search {
        regex,
        ignore_case,
        query,
    }: Search,
) -> Result<(), CliError> {
    const PREFIX_CONTEXT: usize = 40;
    const CONTEXT_WINDOW: usize = 100;

    let (mut database, reader) = open_db()?;
    let mut output = io::stdout().lock();
    let mut print_entry = |entry_id,
                           buf: &[u8],
                           mime_type: &str,
                           start: usize,
                           end: usize|
     -> Result<(), CoreError> {
        writeln!(
            output,
            "--- ENTRY {entry_id}{} ---",
            if mime_type.is_empty() {
                String::new()
            } else {
                format!("; {mime_type}")
            }
        )
        .map_io_err(|| "Failed to write to stdout.")?;

        let bold_start = start.min(PREFIX_CONTEXT);
        let (prefix, suffix) = buf.split_at(bold_start);
        let (middle, suffix) = suffix.split_at((end - start).min(suffix.len()));
        let mut no_empty_write = |buf: &[u8]| -> Result<(), CoreError> {
            if !buf.is_empty() {
                output
                    .write_all(buf)
                    .map_io_err(|| "Failed to write to stdout.")?;
            }
            Ok(())
        };

        no_empty_write(prefix)?;
        no_empty_write(b"\x1b[1m")?;
        no_empty_write(middle)?;
        no_empty_write(b"\x1b[0m")?;
        no_empty_write(suffix)?;
        no_empty_write(b"\n\n")?;

        Ok(())
    };

    let reader = Arc::new(reader);
    let (result_stream, threads) = {
        // TODO https://github.com/rust-lang/rust-clippy/issues/13227
        #[allow(clippy::redundant_locals)]
        let query = query;
        ringboard_sdk::search(
            if regex {
                Query::Regex(Regex::new(&query)?)
            } else if ignore_case {
                Query::PlainIgnoreCase(CaselessQuery::new(query))
            } else {
                Query::Plain(query.as_bytes())
            },
            reader.clone(),
        )
    };
    let mut results = BTreeMap::<BucketAndIndex, (u16, u16)>::new();
    let mut buf = [0; CONTEXT_WINDOW];
    for result in result_stream {
        let QueryResult {
            location,
            start,
            end,
        } = result?;
        match location {
            EntryLocation::Bucketed { bucket, index } => {
                results.insert(
                    BucketAndIndex::new(bucket, index),
                    (u16::try_from(start).unwrap(), u16::try_from(end).unwrap()),
                );
            }
            EntryLocation::File { entry_id } => {
                let entry = unsafe { database.get(entry_id)? };
                let file = entry.to_file_raw(&reader)?.unwrap();

                let remaining = read_at_to_end(
                    &file,
                    buf.as_mut_slice(),
                    u64::try_from(start.saturating_sub(PREFIX_CONTEXT)).unwrap(),
                )
                .map_io_err(|| format!("failed to read from direct entry {entry_id}."))?;

                print_entry(
                    entry_id,
                    &buf[..buf.len() - remaining],
                    &file.mime_type()?,
                    start,
                    end,
                )?;
            }
        }
    }
    for thread in threads {
        thread.join().map_err(|_| CliError::InternalSearchError)?;
    }
    let mut reader = Arc::into_inner(reader).unwrap();

    for entry in database.favorites().chain(database.main()) {
        let Kind::Bucket(bucket) = entry.kind() else {
            continue;
        };
        let Some(&(start, end)) = results.get(&BucketAndIndex::new(
            size_to_bucket(bucket.size()),
            bucket.index(),
        )) else {
            continue;
        };
        let (start, end) = (usize::from(start), usize::from(end));

        let bytes = entry.to_slice(&mut reader)?;
        let prefix_start = start.saturating_sub(PREFIX_CONTEXT);
        print_entry(
            entry.id(),
            &bytes[prefix_start..(prefix_start + CONTEXT_WINDOW).min(bytes.len())],
            &bytes.mime_type()?,
            start,
            end,
        )?;
    }

    Ok(())
}

fn add(
    server: OwnedFd,
    Add {
        data_file,
        favorite,
        mime_type,
        copy,
    }: Add,
) -> Result<(), CliError> {
    let AddResponse::Success { id } = {
        let file = if data_file == Path::new("-") {
            None
        } else {
            Some(
                File::open(&data_file)
                    .map_io_err(|| format!("Failed to open file: {data_file:?}"))?,
            )
        };

        AddRequest::response(
            server,
            if favorite {
                RingKind::Favorites
            } else {
                RingKind::Main
            },
            mime_type
                .or_else(|| {
                    mime_guess::from_path(data_file)
                        .first_raw()
                        .and_then(|s| MimeType::from(s).ok())
                })
                .unwrap_or_default(),
            file.as_ref().map_or(stdin(), |file| file.as_fd()),
        )?
    };

    println!("Entry added: {id}");

    if copy {
        let (mut database, mut reader) = open_db()?;
        let entry = unsafe { database.get(id)? };

        let paste_server = {
            let socket_file = paste_socket_file();
            let addr = SocketAddrUnix::new(&socket_file)
                .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?;
            connect_to_paste_server(&addr)?
        };

        send_paste_buffer(paste_server, entry, &mut reader, false)?;
    }

    Ok(())
}

fn move_to_front(
    server: OwnedFd,
    EntryAction { id }: EntryAction,
    to: Option<RingKind>,
) -> Result<(), CliError> {
    match MoveToFrontRequest::response(server, id, to)? {
        MoveToFrontResponse::Success { id } => {
            println!("Entry moved: {id}");
        }
        MoveToFrontResponse::Error(e) => {
            return Err(e.into());
        }
    }

    Ok(())
}

fn swap(server: OwnedFd, Swap { id1, id2 }: Swap) -> Result<(), CliError> {
    let SwapResponse { error1, error2 } = SwapRequest::response(server, id1, id2)?;
    if let Some(e) = error1 {
        return Err(e.into());
    } else if let Some(e) = error2 {
        return Err(e.into());
    }
    println!("Swapped.");

    Ok(())
}

fn remove(server: OwnedFd, EntryAction { id }: EntryAction) -> Result<(), CliError> {
    let RemoveResponse { error } = RemoveRequest::response(server, id)?;
    if let Some(e) = error {
        return Err(e.into());
    }
    println!("Removed.");

    Ok(())
}

fn wipe() -> Result<(), CliError> {
    let Answer::Yes = ask::ask(
        "⚠️ Are you sure you want to delete your entire clipboard history? ⚠️ [y/N] ",
        Answer::No,
        &mut io::stdin(),
        &mut io::stdout(),
    )
    .map_io_err(|| "Failed to ask for confirmation.")?
    else {
        println!("Aborting.");
        std::process::exit(1)
    };

    let mut data_dir = data_dir();

    data_dir.push(".nuke.server.lock");
    let mut extra_buffer = data_dir.clone();
    data_dir.pop();

    data_dir.push("server.lock");
    acquire_lock_file(
        &mut false,
        CWD,
        data_dir.parent().unwrap(),
        &extra_buffer,
        &data_dir,
        SendQuitAndWait,
    )?;
    data_dir.pop();

    extra_buffer.pop();
    extra_buffer.set_extension("tmp");
    match fs::rename(&data_dir, &extra_buffer) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            println!("Nothing to delete");
            return Ok(());
        }
        r => r,
    }
    .map_io_err(|| format!("Failed to rename dir: {data_dir:?} -> {extra_buffer:?}"))?;

    fuc_engine::remove_dir_all(extra_buffer)?;
    println!("Wiped.");

    Ok(())
}

fn garbage_collect(
    server: OwnedFd,
    GarbageCollect { max_wasted_bytes }: GarbageCollect,
) -> Result<(), CliError> {
    if max_wasted_bytes == 0 {
        let (database, mut reader) = open_db()?;
        let mut duplicates = DuplicateDetector::default();
        let mut num_duplicates = 0;

        let recv = |flags| {
            unsafe { RemoveRequest::recv(&server, flags) }.and_then(
                |Response {
                     sequence_number: _,
                     value: RemoveResponse { error },
                 }| { error.map_or_else(|| Ok(()), |e| Err(e.into())) },
            )
        };
        let mut pending_requests = 0;
        for entry in database.favorites().rev().chain(database.main().rev()) {
            if duplicates.add_entry(&entry, &database, &mut reader)? {
                num_duplicates += 1;
                pipeline_request(
                    |flags| RemoveRequest::send(&server, entry.id(), flags),
                    recv,
                    &mut pending_requests,
                )?;
            }
        }

        drain_requests(recv, 0, &mut pending_requests)?;
        println!("Removed {num_duplicates} duplicate entries.");
    }

    let GarbageCollectResponse { bytes_freed } =
        GarbageCollectRequest::response(server, max_wasted_bytes)?;
    println!("{bytes_freed} bytes of garbage freed.");
    Ok(())
}

fn import(server: OwnedFd, Import { from, database }: Import) -> Result<(), CliError> {
    match from {
        ImportClipboard::GnomeClipboardHistory => migrate_from_gch(server, database),
        ImportClipboard::ClipboardIndicator => migrate_from_clipboard_indicator(server, database),
        ImportClipboard::GPaste => migrate_from_gpaste(server, database),
        ImportClipboard::Json => migrate_from_ringboard_export(server, database.unwrap()),
    }?;
    println!("Migration complete.");
    Ok(())
}

fn migrate_from_gch(server: OwnedFd, database: Option<PathBuf>) -> Result<(), CliError> {
    const OP_TYPE_SAVE_TEXT: u8 = 1;
    const OP_TYPE_DELETE_TEXT: u8 = 2;
    const OP_TYPE_FAVORITE_ITEM: u8 = 3;
    const OP_TYPE_UNFAVORITE_ITEM: u8 = 4;
    const OP_TYPE_MOVE_ITEM_TO_END: u8 = 5;

    fn generate_entry_file(database: impl AsFd, start: u64, len: usize) -> Result<File, CliError> {
        let file = memfd_create(c"ringboard_import_gch", MemfdFlags::empty())
            .map_io_err(|| "Failed to create data entry file.")?;

        let result =
            copy_file_range_all(database, Some(&mut start.clone()), &file, Some(&mut 0), len)
                .map_io_err(|| "Failed to copy data to entry file.")?;
        debug_assert_eq!(len, result);

        Ok(File::from(file))
    }

    let (bytes, database) = {
        let database = database
            .or_else(|| {
                dirs::cache_dir().map(|mut f| {
                    f.push("clipboard-history@alexsaveau.dev/database.log");
                    f
                })
            })
            .ok_or_else(|| io::Error::from(ErrorKind::NotFound))
            .map_io_err(|| "Failed to find Gnome Clipboard History database file")?;

        let file =
            File::open(&database).map_io_err(|| format!("Failed to open file: {database:?}"))?;
        (
            Mmap::from(&file).map_io_err(|| format!("Failed to mmap file: {database:?}"))?,
            file,
        )
    };

    let mut translation = Vec::new();
    let mut pending_adds = 0;
    let mut i = 0;
    while i < bytes.len() {
        macro_rules! gch_id {
            () => {{
                let gch_id = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
                // GCH uses one indexing
                usize::try_from(gch_id - 1).unwrap()
            }};
        }
        macro_rules! get_translation {
            () => {{
                let gch_id = gch_id!();
                if translation.len() <= gch_id {
                    unsafe {
                        drain_add_requests(&server, Some(&mut translation), &mut pending_adds)?;
                    }
                }
                translation[gch_id]
            }};
        }
        macro_rules! api_error {
            ($e:expr) => {
                println!(
                    "GCH database may be corrupted or ringboard database may be too small (so \
                     there were collisions)."
                );
                return Err($e.into());
            };
        }

        let op = bytes[i];
        i += 1;
        match op {
            OP_TYPE_SAVE_TEXT => {
                let raw_len = bytes[i..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or_else(|| io::Error::from(ErrorKind::InvalidData))
                    .map_io_err(|| "GCH database corrupted: data was not NUL terminated")?;

                let data = generate_entry_file(&database, u64::try_from(i).unwrap(), raw_len)?;
                i += 1 + raw_len;

                unsafe {
                    pipeline_add_request(
                        &server,
                        data,
                        RingKind::Main,
                        MimeType::new_const(),
                        Some(&mut translation),
                        &mut pending_adds,
                    )?;
                }
            }
            OP_TYPE_DELETE_TEXT => {
                if let RemoveResponse { error: Some(e) } =
                    RemoveRequest::response(&server, get_translation!())?
                {
                    api_error!(e);
                }
                i += 4;
            }
            OP_TYPE_FAVORITE_ITEM | OP_TYPE_UNFAVORITE_ITEM | OP_TYPE_MOVE_ITEM_TO_END => {
                match MoveToFrontRequest::response(&server, get_translation!(), match op {
                    OP_TYPE_FAVORITE_ITEM => Some(RingKind::Favorites),
                    OP_TYPE_UNFAVORITE_ITEM => Some(RingKind::Main),
                    OP_TYPE_MOVE_ITEM_TO_END => None,
                    _ => unreachable!(),
                })? {
                    MoveToFrontResponse::Success { id } => {
                        translation[gch_id!()] = id;
                    }
                    MoveToFrontResponse::Error(e) => {
                        api_error!(e);
                    }
                }
                i += 4;
            }
            _ => {
                Err(io::Error::from(ErrorKind::InvalidData)).map_io_err(|| {
                    format!("GCH database corrupted: unknown operation {:?}", bytes[i])
                })?;
                unreachable!();
            }
        }
    }

    unsafe { drain_add_requests(server, None, &mut pending_adds) }
}

fn migrate_from_clipboard_indicator(
    server: OwnedFd,
    database: Option<PathBuf>,
) -> Result<(), CliError> {
    #[derive(Deserialize)]
    struct Entry {
        #[serde(default)]
        favorite: bool,
        #[serde(default)]
        mimetype: MimeType,
        #[serde(default)]
        contents: String,
    }

    fn generate_entry_file(data: &str) -> Result<File, CliError> {
        let file = File::from(
            memfd_create(c"ringboard_clipboard_indicator", MemfdFlags::empty())
                .map_io_err(|| "Failed to create data entry file.")?,
        );

        file.write_all_at(data.as_bytes(), 0)
            .map_io_err(|| "Failed to copy data to entry file.")?;

        Ok(file)
    }

    let database_dir = {
        let database = database
            .or_else(|| {
                dirs::cache_dir().map(|mut f| {
                    f.push("clipboard-indicator@tudmotu.com");
                    f
                })
            })
            .ok_or_else(|| io::Error::from(ErrorKind::NotFound))
            .map_io_err(|| "Failed to find Clipboard Indicator directory path.")?;
        openat(
            CWD,
            &*database,
            OFlags::DIRECTORY | OFlags::PATH,
            Mode::empty(),
        )
        .map_io_err(|| format!("Failed to open directory: {database:?}"))?
    };
    let registry_file = File::from(
        openat(
            &database_dir,
            c"registry.txt",
            OFlags::RDONLY,
            Mode::empty(),
        )
        .map_io_err(|| "Failed to open registry file.")?,
    );

    let mut pending_adds = 0;
    for Entry {
        favorite,
        mimetype,
        ref contents,
    } in serde_json::from_reader::<_, Vec<Entry>>(BufReader::new(registry_file))?
    {
        if contents.is_empty() {
            continue;
        }

        // https://github.com/Tudmotu/gnome-shell-extension-clipboard-indicator/blob/46442690f0a6fd2a4caef1851582155af6fd5976/registry.js#L31-L38
        let data = if mimetype.is_empty()
            || mimetype.starts_with("text/")
            || &mimetype == "STRING"
            || &mimetype == "UTF8_STRING"
        {
            generate_entry_file(contents)?
        } else if mimetype.starts_with("image/") {
            let contents = contents.rsplit('/').next().unwrap_or(contents);
            File::from(
                openat(&database_dir, contents, OFlags::RDONLY, Mode::empty())
                    .map_io_err(|| format!("Failed to open data file: {contents:?}"))?,
            )
        } else {
            continue;
        };

        unsafe {
            pipeline_add_request(
                &server,
                data,
                if favorite {
                    RingKind::Favorites
                } else {
                    RingKind::Main
                },
                mimetype,
                None,
                &mut pending_adds,
            )?;
        }
    }

    unsafe { drain_add_requests(server, None, &mut pending_adds) }
}

fn migrate_from_gpaste(server: OwnedFd, database: Option<PathBuf>) -> Result<(), CliError> {
    #[derive(Deserialize, Debug)]
    struct History {
        #[serde(rename = "@version")]
        _version: String,
        #[serde(rename = "item")]
        items: Vec<Item>,
    }

    #[derive(Deserialize, Debug)]
    enum ItemKind {
        Text,
        Image,
        Uris,
    }

    #[derive(Deserialize, Debug)]
    struct Item {
        #[serde(rename = "@kind")]
        kind: ItemKind,
        #[serde(rename = "value")]
        values: Vec<String>,
    }

    fn generate_entry_file(data: &str) -> Result<File, CliError> {
        let file = File::from(
            memfd_create(c"ringboard_gpaste", MemfdFlags::empty())
                .map_io_err(|| "Failed to create data entry file.")?,
        );

        file.write_all_at(data.as_bytes(), 0)
            .map_io_err(|| "Failed to copy data to entry file.")?;

        Ok(file)
    }

    let database_dir = {
        let database = database
            .or_else(|| {
                dirs::data_local_dir().map(|mut f| {
                    f.push("gpaste");
                    f
                })
            })
            .ok_or_else(|| io::Error::from(ErrorKind::NotFound))
            .map_io_err(|| "Failed to find GPaste directory path.")?;
        openat(
            CWD,
            &*database,
            OFlags::DIRECTORY | OFlags::PATH,
            Mode::empty(),
        )
        .map_io_err(|| format!("Failed to open directory: {database:?}"))?
    };
    let mut history_file = File::from(
        openat(&database_dir, c"history.xml", OFlags::RDONLY, Mode::empty())
            .map_io_err(|| "Failed to open history file.")?,
    );
    let images_dir = openat(
        database_dir,
        c"images",
        OFlags::DIRECTORY | OFlags::PATH,
        Mode::empty(),
    )
    .map_io_err(|| "Failed to open images dir")?;

    {
        let mut reader = quick_xml::Reader::from_reader(BufReader::new(&history_file));
        let mut buf = Vec::new();
        let unsupported = Err(io::Error::from(ErrorKind::Unsupported))
            .map_io_err(|| "The GPaste v2.0 data format is the only one currently supported.");
        loop {
            use quick_xml::events::Event;
            match reader.read_event_into(&mut buf)? {
                Event::Eof => {
                    return Err(io::Error::from(ErrorKind::UnexpectedEof))
                        .map_io_err(|| "GPaste history file appears to be corrupted.")?;
                }
                Event::Start(e) => {
                    return match e.name().as_ref() {
                        b"history" => {
                            if e.try_get_attribute("version")?
                                .is_some_and(|s| s.value.as_ref() == b"2.0")
                            {
                                break;
                            }
                            unsupported?
                        }
                        _ => unsupported?,
                    };
                }
                _ => (),
            }
            buf.clear();
        }
    }

    history_file
        .seek(SeekFrom::Start(0))
        .map_io_err(|| "Failed to reset history file offset.")?;
    let mut pending_adds = 0;
    for Item { kind, values } in
        quick_xml::de::from_reader::<_, History>(BufReader::new(history_file))?
            .items
            .into_iter()
            .rev()
    {
        let Some(value) = values.first() else {
            continue;
        };
        if value.is_empty() {
            continue;
        }

        let (data, mime) = match kind {
            ItemKind::Text | ItemKind::Uris => (generate_entry_file(value)?, MimeType::new_const()),
            ItemKind::Image => (
                {
                    let value = value.rsplit('/').next().unwrap_or(value);
                    File::from(
                        openat(&images_dir, value, OFlags::RDONLY, Mode::empty())
                            .map_io_err(|| format!("Failed to open data file: {value:?}"))?,
                    )
                },
                // https://github.com/Keruspe/GPaste/blob/3a88a878328dfddae712f8dfe2d351f08b356d50/src/daemon/tmp/gpaste-image-item.c#L278
                MimeType::from("image/png").unwrap(),
            ),
        };

        unsafe {
            pipeline_add_request(&server, data, RingKind::Main, mime, None, &mut pending_adds)?;
        }
    }

    unsafe { drain_add_requests(server, None, &mut pending_adds) }
}

#[allow(clippy::cast_precision_loss)]
fn stats() -> Result<(), CliError> {
    #[derive(Default, Debug)]
    struct RingStats {
        capacity: u32,
        len: u32,
        bucketed_entry_count: u32,
        file_entry_count: u32,
        num_duplicates: u32,
        min_entry_size: u64,
        max_entry_size: u64,
        owned_bytes: u64,
    }

    #[derive(Default, Debug)]
    struct BucketStats {
        size_class: usize,

        num_slots: u32,
        used_slots: u32,
        owned_bytes: u64,
    }

    #[derive(Default, Debug)]
    struct DirectFileStats {
        owned_bytes: u64,
        allocated_bytes: u64,
        mime_types: BTreeMap<MimeType, u32>,
    }

    #[derive(Default, Debug)]
    struct Stats {
        rings: HashMap<RingKind, RingStats, BuildHasherDefault<FxHasher>>,
        buckets: [BucketStats; NUM_BUCKETS],
        direct_files: DirectFileStats,
    }

    impl Display for Stats {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            let mut s = f.debug_struct("Stats");

            s.field_with("raw", |f| {
                f.debug_struct("Raw")
                    .field("rings", &self.rings)
                    .field("buckets", &self.buckets)
                    .field("direct_files", &self.direct_files)
                    .finish()
            });
            s.field_with("computed", |f| {
                f.debug_struct("Computed")
                    .field_with("rings", |f| {
                        let mut rings = f.debug_map();
                        for (
                            kind,
                            &RingStats {
                                capacity: _,
                                len,
                                bucketed_entry_count,
                                file_entry_count,
                                num_duplicates: _,
                                min_entry_size: _,
                                max_entry_size: _,
                                owned_bytes,
                            },
                        ) in &self.rings
                        {
                            rings.key(kind).value_with(|f| {
                                let num_entries = bucketed_entry_count + file_entry_count;
                                let mut s = f.debug_struct("Ring");
                                s.field("num_entries", &num_entries)
                                    .field("uninitialized_entry_count", &(len - num_entries))
                                    .field(
                                        "mean_entry_size",
                                        &(owned_bytes as f64 / f64::from(num_entries)),
                                    );
                                s.finish()
                            });
                        }
                        rings.finish()
                    })
                    .field_with("buckets", |f| {
                        let mut buckets = f.debug_map();
                        for &BucketStats {
                            size_class,
                            num_slots,
                            used_slots,
                            owned_bytes,
                        } in &self.buckets
                        {
                            let length = bucket_to_length(size_class - 2);
                            let used_bytes = u64::from(length) * u64::from(used_slots);
                            let fragmentation = used_bytes - owned_bytes;
                            buckets.key(&length).value_with(|f| {
                                f.debug_struct("Bucket")
                                    .field("free_slots", &(num_slots - used_slots))
                                    .field("fragmentation_bytes", &fragmentation)
                                    .field(
                                        "fragmentation_ratio",
                                        &(fragmentation as f64 / used_bytes as f64),
                                    )
                                    .finish()
                            });
                        }
                        buckets.finish()
                    })
                    .field_with("direct_files", |f| {
                        let &DirectFileStats {
                            owned_bytes,
                            allocated_bytes,
                            mime_types: _,
                        } = &self.direct_files;
                        f.debug_struct("DirectFiles")
                            .field(
                                "fragmentation_ratio",
                                &((allocated_bytes - owned_bytes) as f64 / allocated_bytes as f64),
                            )
                            .finish()
                    })
                    .finish()
            });

            s.finish()
        }
    }

    let mut stats = Stats::default();
    let Stats {
        rings,
        buckets,
        direct_files:
            DirectFileStats {
                owned_bytes: direct_owned_bytes,
                allocated_bytes,
                mime_types,
            },
    } = &mut stats;

    let (database, mut reader) = open_db()?;
    let mut duplicates = DuplicateDetector::default();

    for (
        i,
        (
            BucketStats {
                size_class,
                num_slots,
                used_slots: _,
                owned_bytes: _,
            },
            mem,
        ),
    ) in buckets.iter_mut().zip(reader.buckets()).enumerate()
    {
        *size_class = i + 2;
        *num_slots = u32::try_from(mem.len() / usize::from(bucket_to_length(i))).unwrap();
    }

    for ring_reader in [database.favorites(), database.main()] {
        let mut ring_stats = RingStats::default();
        let RingStats {
            capacity,
            len,
            bucketed_entry_count,
            file_entry_count,
            num_duplicates,
            min_entry_size,
            max_entry_size,
            owned_bytes: ring_owned_bytes,
        } = &mut ring_stats;
        *capacity = ring_reader.ring().capacity();
        *len = ring_reader.ring().len();
        *min_entry_size = u64::MAX;
        let kind = ring_reader.kind();

        for entry in ring_reader {
            let entry_size;
            let duplicate;

            match entry.kind() {
                Kind::Bucket(bucket) => {
                    *bucketed_entry_count += 1;

                    let BucketStats {
                        size_class: _,
                        num_slots: _,
                        used_slots,
                        owned_bytes,
                    } = &mut buckets[usize::from(size_to_bucket(bucket.size()))];
                    *used_slots += 1;

                    entry_size = u64::from(bucket.size());
                    *owned_bytes += entry_size;

                    duplicate = duplicates.add_entry(&entry, &database, &mut reader)?;
                }
                Kind::File => {
                    *file_entry_count += 1;

                    let file = entry.to_file(&mut reader)?;
                    let stats = statx(
                        &*file,
                        c"",
                        AtFlags::EMPTY_PATH,
                        StatxFlags::SIZE | StatxFlags::BLOCKS,
                    )
                    .map_io_err(|| format!("Failed to statx file: {file:?}"))?;

                    entry_size = stats.stx_size;
                    *direct_owned_bytes += entry_size;
                    *mime_types.entry(file.mime_type()?).or_default() += 1;
                    *allocated_bytes += stats.stx_blocks * 512;

                    duplicate = duplicates.add_entry(&entry, &database, &mut reader)?;
                }
            }

            *ring_owned_bytes += entry_size;
            *min_entry_size = min(*min_entry_size, entry_size);
            *max_entry_size = max(*max_entry_size, entry_size);
            if duplicate {
                *num_duplicates += 1;
            }
        }

        rings.insert(kind, ring_stats);
    }

    println!("{stats:#}");

    Ok(())
}

base64_serde_type!(
    Base64Standard,
    base64::engine::general_purpose::STANDARD_NO_PAD
);

#[derive(Serialize, Deserialize)]
#[serde(bound(deserialize = "'de: 'a"))]
struct ExportEntry<'a> {
    #[serde(default)]
    id: u64,
    #[serde(flatten)]
    data: ExportData<'a>,
    #[serde(skip_serializing_if = "MimeType::is_empty")]
    #[serde(default)]
    mime_type: MimeType,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
enum ExportData<'a> {
    Human(Cow<'a, str>),
    Bytes(#[serde(with = "Base64Standard")] Cow<'a, [u8]>),
}

fn dump() -> Result<(), CliError> {
    let (database, mut reader) = open_db()?;
    let mut seq = serde_json::Serializer::new(io::stdout().lock());
    let mut seq = seq.serialize_seq(None)?;
    for entry in database.favorites().chain(database.main()) {
        let loaded = entry.to_slice(&mut reader)?;
        let mime_type = loaded.mime_type()?;
        seq.serialize_element(&ExportEntry {
            id: entry.id(),
            data: str::from_utf8(&loaded).map_or_else(
                |_| ExportData::Bytes((&**loaded).into()),
                |data| ExportData::Human(data.into()),
            ),
            mime_type,
        })?;
    }

    SerializeSeq::end(seq)?;
    Ok(())
}

fn migrate_from_ringboard_export(server: OwnedFd, dump_file: PathBuf) -> Result<(), CliError> {
    fn generate_entry_file(tmp_file_unsupported: &mut bool, data: &[u8]) -> Result<File, CliError> {
        let file = File::from(
            create_tmp_file(
                tmp_file_unsupported,
                CWD,
                c".",
                c".ringboard-import-scratchpad",
                OFlags::RDWR,
                Mode::empty(),
            )
            .map_io_err(|| "Failed to create data entry file.")?,
        );

        file.write_all_at(data, 0)
            .map_io_err(|| "Failed to copy data to entry file.")?;

        Ok(file)
    }

    let mut pending_adds = 0;
    let mut cache = Default::default();
    let mut process = |ExportEntry {
                           id,
                           data,
                           mime_type,
                       }|
     -> Result<(), CliError> {
        let data = generate_entry_file(&mut cache, match &data {
            ExportData::Human(str) => str.as_bytes(),
            ExportData::Bytes(bytes) => bytes,
        })?;

        let (to, _) = decompose_id(id).unwrap_or_default();
        unsafe { pipeline_add_request(&server, data, to, mime_type, None, &mut pending_adds) }
    };

    if dump_file == Path::new("-") {
        drop(dump_file);
        let iter =
            serde_json::Deserializer::from_reader(io::stdin().lock()).into_iter::<ExportEntry>();
        for result in iter {
            process(result?)?;
        }
    } else {
        let dump =
            File::open(&dump_file).map_io_err(|| format!("Failed to open file: {dump_file:?}"))?;
        drop(dump_file);

        let iter =
            serde_json::Deserializer::from_reader(BufReader::new(dump)).into_iter::<ExportEntry>();
        for result in iter {
            process(result?)?;
        }
    };

    unsafe { drain_add_requests(server, None, &mut pending_adds) }
}

fn generate(
    server: OwnedFd,
    Generate {
        num_entries,
        mean_size,
        cv_size,
    }: Generate,
) -> Result<(), CliError> {
    fn generate_random_entry_file(
        rng: &mut (impl RngCore + 'static),
        len_distr: LogNormal<f64>,
    ) -> Result<(File, u64), CliError> {
        let mut file = File::from(
            memfd_create(c"ringboard_gen", MemfdFlags::empty())
                .map_io_err(|| "Failed to create data entry file.")?,
        );

        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let len = len_distr.sample(rng).round().max(1.) as u64;
        // TODO use adapter when it's available
        let result = io::copy(&mut (rng as &mut dyn RngCore).take(len), &mut file)
            .map_io_err(|| "Failed to write bytes to entry file.")?;
        debug_assert_eq!(len, result);
        file.seek(SeekFrom::Start(0))
            .map_io_err(|| "Failed to reset entry file offset.")?;

        Ok((file, len))
    }

    struct GenerateRingKind(RingKind);

    impl Distribution<GenerateRingKind> for Standard {
        fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> GenerateRingKind {
            match rng.gen_range(0..100) {
                0 => GenerateRingKind(RingKind::Favorites),
                _ => GenerateRingKind(RingKind::Main),
            }
        }
    }

    let distr = LogNormal::from_mean_cv(f64::from(mean_size), f64::from(cv_size)).unwrap();
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(u64::from(num_entries));
    let mut pending_adds = 0;

    for _ in 0..num_entries {
        let data = generate_random_entry_file(&mut rng, distr)?.0;
        unsafe {
            pipeline_add_request(
                &server,
                data,
                rng.gen::<GenerateRingKind>().0,
                MimeType::new_const(),
                None,
                &mut pending_adds,
            )?;
        }
    }

    unsafe { drain_add_requests(server, None, &mut pending_adds) }
}

fn fuzz(
    addr: &SocketAddrUnix,
    Fuzz {
        seed,
        mean_size,
        cv_size,
        verbose,
    }: Fuzz,
) -> Result<(), CliError> {
    fn generate_entry_file(data: &[u8]) -> Result<File, CliError> {
        let file = File::from(
            memfd_create(c"ringboard_fuzz", MemfdFlags::empty())
                .map_io_err(|| "Failed to create data entry file.")?,
        );

        file.write_all_at(data, 0)
            .map_io_err(|| "Failed to copy data to entry file.")?;

        Ok(file)
    }

    struct FuzzRingKind(RingKind);

    impl Distribution<FuzzRingKind> for Standard {
        fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> FuzzRingKind {
            match rng.gen_range(0..=2) {
                0 => FuzzRingKind(RingKind::Favorites),
                _ => FuzzRingKind(RingKind::Main),
            }
        }
    }

    struct NoDebug<T>(T);

    impl<T> Debug for NoDebug<T> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("skipped").finish()
        }
    }

    #[derive(Debug)]
    enum PendingOp {
        Add { data: NoDebug<Vec<u8>> },
        Move { id: u64 },
        Swap { id1: u64, id2: u64 },
        Remove { id: u64 },
        Gc,
    }

    #[derive(Debug)]
    enum ResponseKind {
        Add {
            data: NoDebug<Vec<u8>>,
            value: AddResponse,
        },
        Move {
            move_id: u64,
            value: MoveToFrontResponse,
        },
        Swap {
            id1: u64,
            id2: u64,
            value: SwapResponse,
        },
        Remove {
            id: u64,
            value: RemoveResponse,
        },
        Gc(GarbageCollectResponse),
    }

    fn recv(
        mut out: impl Write,
        flags: RecvFlags,
        server: impl AsFd,
        pending: &mut VecDeque<PendingOp>,
        database: &mut HashMap<u64, Vec<u8>, BuildHasherDefault<FxHasher>>,
        linearized_responses: &mut VecDeque<Option<ResponseKind>>,
        seq_num_head: &mut u64,
        verbose: bool,
    ) -> Result<(), ClientError> {
        macro_rules! linearize {
            ($seq:expr, $resp:expr) => {
                let slot = usize::try_from($seq - *seq_num_head).unwrap();
                if slot >= linearized_responses.len() {
                    linearized_responses.resize_with(slot + 1, Default::default);
                }
                linearized_responses[slot] = Some($resp);
            };
        }
        match pending.front().unwrap() {
            PendingOp::Add { .. } => {
                let Response {
                    sequence_number,
                    value,
                } = unsafe { AddRequest::recv(server, flags) }?;
                let PendingOp::Add { data } = pending.pop_front().unwrap() else {
                    unreachable!()
                };
                linearize!(sequence_number, ResponseKind::Add { value, data });
            }
            PendingOp::Move { .. } => {
                let Response {
                    sequence_number,
                    value,
                } = unsafe { MoveToFrontRequest::recv(server, flags) }?;
                let PendingOp::Move { id: move_id } = pending.pop_front().unwrap() else {
                    unreachable!()
                };
                linearize!(sequence_number, ResponseKind::Move { move_id, value });
            }
            PendingOp::Swap { .. } => {
                let Response {
                    sequence_number,
                    value,
                } = unsafe { SwapRequest::recv(server, flags) }?;
                let PendingOp::Swap { id1, id2 } = pending.pop_front().unwrap() else {
                    unreachable!()
                };
                linearize!(sequence_number, ResponseKind::Swap { id1, id2, value });
            }
            PendingOp::Remove { .. } => {
                let Response {
                    sequence_number,
                    value,
                } = unsafe { RemoveRequest::recv(server, flags) }?;
                let PendingOp::Remove { id } = pending.pop_front().unwrap() else {
                    unreachable!()
                };
                linearize!(sequence_number, ResponseKind::Remove { id, value });
            }
            PendingOp::Gc => {
                let Response {
                    sequence_number,
                    value,
                } = unsafe { GarbageCollectRequest::recv(server, flags) }?;
                let PendingOp::Gc = pending.pop_front().unwrap() else {
                    unreachable!()
                };
                linearize!(sequence_number, ResponseKind::Gc(value));
            }
        }

        while linearized_responses.front().is_some_and(Option::is_some) {
            let kind = linearized_responses.pop_front().unwrap().unwrap();
            if verbose {
                writeln!(out, "{seq_num_head}@{kind:?}.")
                    .map_io_err(|| "Failed to write to stdout.")?;
            }
            *seq_num_head = seq_num_head.wrapping_add(1);

            match kind {
                ResponseKind::Add {
                    data: NoDebug(data),
                    value: AddResponse::Success { id },
                } => {
                    database.insert(id, data);
                }
                ResponseKind::Move { move_id, value } => match value {
                    MoveToFrontResponse::Success { id } => {
                        let file = database.remove(&move_id).unwrap();
                        database.insert(id, file);
                    }
                    MoveToFrontResponse::Error(_) => {
                        assert!(!database.contains_key(&move_id));
                    }
                },
                ResponseKind::Swap { id1, id2, value } => match value {
                    SwapResponse {
                        error1: None,
                        error2: None,
                    } => {
                        let file1 = database.remove(&id1);
                        let file2 = database.remove(&id2);
                        assert!(file1.is_some() || file2.is_some());

                        if let Some(file2) = file2 {
                            database.insert(id1, file2);
                        }
                        if let Some(file1) = file1 {
                            database.insert(id2, file1);
                        }
                    }
                    SwapResponse { error1, error2 } => {
                        if error1.is_some() {
                            assert!(!database.contains_key(&id1));
                        }
                        if error2.is_some() {
                            assert!(!database.contains_key(&id2));
                        }
                    }
                },
                ResponseKind::Remove { id, value } => match value {
                    RemoveResponse { error: None } => {
                        assert!(database.remove(&id).is_some());
                    }
                    RemoveResponse { error: Some(_) } => {
                        assert!(!database.contains_key(&id));
                    }
                },
                ResponseKind::Gc(GarbageCollectResponse { bytes_freed: _ }) => {}
            }
        }

        Ok(())
    }

    let distr =
        WeightedAliasIndex::new(vec![550u32, 450, 50000, 20000, 20000, 1000, 100, 10]).unwrap();
    let entry_size_distr =
        LogNormal::from_mean_cv(f64::from(mean_size), f64::from(cv_size)).unwrap();
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut buf = String::with_capacity(MimeType::new_const().capacity());

    let mut clients = ArrayVec::<_, 32>::new_const();
    let mut pending_ops = [const { VecDeque::new() }; 32];
    let mut pending_requests = [0; 32];
    let mut data = HashMap::default();
    let mut sequence_num = 1;
    let mut linearizable_ops = VecDeque::new();

    let (mut database, mut reader) = open_db()?;
    let mut out = io::stdout().lock();
    loop {
        match distr.sample(&mut rng) {
            0 => {
                if let Ok(client) = if clients.len() == 32 {
                    connect_to_server_with(addr, SocketFlags::NONBLOCK)
                } else if clients.is_empty() {
                    Ok(connect_to_server(addr)?)
                } else {
                    connect_to_server(addr)
                } {
                    if verbose {
                        writeln!(out, "Client {} connected.", clients.len())
                            .map_io_err(|| "Failed to write to stdout.")?;
                    }
                    clients.push(client);
                }
            }
            1 => {
                if !clients.is_empty() {
                    let idx = rng.gen_range(0..clients.len());
                    if verbose {
                        writeln!(out, "Closing client {idx}.")
                            .map_io_err(|| "Failed to write to stdout.")?;
                    }

                    drain_requests(
                        |flags| {
                            recv(
                                &mut out,
                                flags,
                                &clients[idx],
                                &mut pending_ops[idx],
                                &mut data,
                                &mut linearizable_ops,
                                &mut sequence_num,
                                verbose,
                            )
                        },
                        0,
                        &mut pending_requests[idx],
                    )?;

                    clients.swap_remove(idx);
                    pending_ops.swap(idx, clients.len());
                    pending_requests.swap(idx, clients.len());
                }
            }
            action @ 2..=6 => {
                if clients.is_empty() {
                    if verbose {
                        writeln!(out, "Client {} connected.", clients.len())
                            .map_io_err(|| "Failed to write to stdout.")?;
                    }
                    clients.push(connect_to_server(addr)?);
                }
                let (server, pending_ops, pending_requests) = {
                    let idx = rng.gen_range(0..clients.len());
                    (
                        &clients[idx],
                        &mut pending_ops[idx],
                        &mut pending_requests[idx],
                    )
                };
                macro_rules! pipeline_request {
                    ($send:expr) => {
                        pipeline_request(
                            $send,
                            |flags| {
                                recv(
                                    &mut out,
                                    flags,
                                    server,
                                    pending_ops,
                                    &mut data,
                                    &mut linearizable_ops,
                                    &mut sequence_num,
                                    verbose,
                                )
                            },
                            pending_requests,
                        )?;
                    };
                }

                match action {
                    2 => {
                        let kind = rng.gen::<FuzzRingKind>().0;
                        let mime_type = if rng.gen_range(0..50) == 0 {
                            let len = rng.gen_range(1..=MimeType::new_const().capacity());
                            Alphanumeric.append_string(&mut rng, &mut buf, len);

                            let mime = MimeType::from(&buf).unwrap();
                            buf.clear();
                            mime
                        } else {
                            MimeType::new_const()
                        };

                        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                        let len = entry_size_distr.sample(&mut rng).round().max(1.) as usize;
                        let data = Alphanumeric
                            .sample_iter(&mut rng)
                            .take(len)
                            .collect::<Vec<_>>();
                        let file = generate_entry_file(&data)?;
                        pending_ops.push_back(PendingOp::Add {
                            data: NoDebug(data),
                        });
                        pipeline_request!(|flags| AddRequest::send(
                            server, kind, mime_type, &file, flags
                        ));
                    }
                    3 => {
                        let move_id = rng.gen_range(0..=sequence_num);
                        let kind = rng.gen::<Option<FuzzRingKind>>().map(|r| r.0);

                        pending_ops.push_back(PendingOp::Move { id: move_id });
                        pipeline_request!(|flags| MoveToFrontRequest::send(
                            server, move_id, kind, flags
                        ));
                    }
                    4 => {
                        let id1 = rng.gen_range(0..=sequence_num);
                        let id2 = rng.gen_range(0..=sequence_num);

                        pending_ops.push_back(PendingOp::Swap { id1, id2 });
                        pipeline_request!(|flags| SwapRequest::send(server, id1, id2, flags));
                    }
                    5 => {
                        let id = rng.gen_range(0..=sequence_num);

                        pending_ops.push_back(PendingOp::Remove { id });
                        pipeline_request!(|flags| RemoveRequest::send(server, id, flags));
                    }
                    6 => {
                        let max_wasted_bytes = match rng.gen_range(0..4) {
                            0 => 0,
                            _ => rng.gen_range(0..10_000) + 1,
                        };

                        pending_ops.push_back(PendingOp::Gc);
                        pipeline_request!(|flags| GarbageCollectRequest::send(
                            server,
                            max_wasted_bytes,
                            flags
                        ));
                    }
                    _ => unreachable!(),
                }
            }
            7 => {
                writeln!(
                    out,
                    "Validating database integrity on {} entries.",
                    data.len()
                )
                .map_io_err(|| "Failed to write to stdout.")?;

                for ((server, pending_ops), pending_requests) in clients
                    .iter()
                    .zip(&mut pending_ops)
                    .zip(&mut pending_requests)
                {
                    drain_requests(
                        |flags| {
                            recv(
                                &mut out,
                                flags,
                                server,
                                pending_ops,
                                &mut data,
                                &mut linearizable_ops,
                                &mut sequence_num,
                                verbose,
                            )
                        },
                        0,
                        pending_requests,
                    )?;
                    debug_assert!(pending_ops.is_empty());
                    debug_assert_eq!(*pending_requests, 0);
                }
                debug_assert!(linearizable_ops.is_empty());

                for (&id, a) in &data {
                    let entry = unsafe { database.get(id) }?;
                    let b = &**entry.to_slice(&mut reader)?;

                    assert_eq!(**a, *b, "{entry:?}");
                }
            }
            _ => unreachable!(),
        }
    }
}

fn configure_x11(ConfigureX11 { auto_paste }: ConfigureX11) -> Result<(), CliError> {
    let path = x11_config_file();
    {
        let parent = path.parent().unwrap();
        create_dir_all(parent).map_io_err(|| format!("Failed to create dir: {parent:?}"))?;
    }
    let mut file = File::create(&path).map_io_err(|| format!("Failed to open file: {path:?}"))?;

    let config = toml::to_string_pretty(&X11Config::V1(X11V1Config { auto_paste }))?;
    file.write_all(config.as_bytes())
        .map_io_err(|| format!("Failed to write to config file: {path:?}"))?;

    println!("Saved configuration file to {path:?}.");
    Ok(())
}

fn pipeline_request(
    mut send: impl FnMut(SendFlags) -> Result<(), ClientError>,
    mut recv: impl FnMut(RecvFlags) -> Result<(), ClientError>,
    pending_requests: &mut u32,
) -> Result<(), CliError> {
    if *pending_requests >= 8 {
        drain_requests(&mut recv, *pending_requests / 2, pending_requests)?;
    }

    loop {
        match send(if *pending_requests == 0 {
            SendFlags::empty()
        } else {
            SendFlags::DONTWAIT
        }) {
            Err(ClientError::Core(CoreError::Io { error: e, .. }))
                if e.kind() == ErrorKind::WouldBlock =>
            {
                debug_assert!(*pending_requests > 0);
                drain_requests(&mut recv, *pending_requests / 2, pending_requests)?;
            }
            r => {
                r?;
                *pending_requests += 1;
                break;
            }
        };
    }
    Ok(())
}

fn drain_requests(
    mut recv: impl FnMut(RecvFlags) -> Result<(), ClientError>,
    remaining: u32,
    pending_requests: &mut u32,
) -> Result<(), CliError> {
    while *pending_requests > 0 {
        match recv(if *pending_requests > remaining {
            RecvFlags::empty()
        } else {
            RecvFlags::DONTWAIT
        }) {
            Err(ClientError::Core(CoreError::Io { error: e, .. }))
                if e.kind() == ErrorKind::WouldBlock =>
            {
                debug_assert!(*pending_requests <= remaining);
                break;
            }
            r => r?,
        };
        *pending_requests -= 1;
    }
    Ok(())
}

fn pipelined_add_recv<'a>(
    server: impl AsFd + 'a,
    mut translation: Option<&'a mut Vec<u64>>,
) -> impl FnMut(RecvFlags) -> Result<(), ClientError> + 'a {
    move |flags| {
        unsafe { AddRequest::recv(&server, flags) }.map(
            |Response {
                 sequence_number: _,
                 value: AddResponse::Success { id },
             }| {
                if let Some(translation) = translation.as_deref_mut() {
                    translation.push(id);
                }
            },
        )
    }
}

unsafe fn pipeline_add_request(
    server: impl AsFd + Copy,
    data: impl AsFd,
    to: RingKind,
    mime_type: MimeType,
    translation: Option<&mut Vec<u64>>,
    pending_adds: &mut u32,
) -> Result<(), CliError> {
    pipeline_request(
        |flags| AddRequest::send(server, to, mime_type, &data, flags),
        pipelined_add_recv(server, translation),
        pending_adds,
    )
}

unsafe fn drain_add_requests(
    server: impl AsFd,
    translation: Option<&mut Vec<u64>>,
    pending_adds: &mut u32,
) -> Result<(), CliError> {
    drain_requests(pipelined_add_recv(server, translation), 0, pending_adds)
}

#[cfg(test)]
mod cli_tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn verify_app() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_for_review() {
        supercilex_tests::help_for_review(Cli::command());
    }
}
