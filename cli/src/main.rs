#![feature(debug_closure_helpers)]

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::{Debug, Display, Formatter},
    fs,
    fs::File,
    hash::BuildHasherDefault,
    io,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::FileExt,
    },
    path::{Path, PathBuf},
    str,
};

use ask::Answer;
use base64_serde::base64_serde_type;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, ValueHint};
use clap_num::si_number;
use error_stack::Report;
use memmap2::Mmap;
use rand::{
    distributions::{Alphanumeric, DistString, Standard},
    Rng,
};
use rand_distr::{Distribution, LogNormal, WeightedAliasIndex};
use rand_xoshiro::{
    rand_core::{RngCore, SeedableRng},
    Xoshiro256PlusPlus,
};
use ringboard_core::{
    bucket_to_length, copy_file_range_all,
    dirs::{data_dir, socket_file},
    protocol::{
        AddResponse, GarbageCollectResponse, IdNotFoundError, MimeType, MoveToFrontResponse,
        RemoveResponse, RingKind, SwapResponse,
    },
    read_server_pid, size_to_bucket, IoErr,
};
use ringboard_sdk::{connect_to_server, connect_to_server_with, DatabaseReader, EntryReader, Kind};
use rustc_hash::FxHasher;
use rustix::{
    event::{poll, PollFd, PollFlags},
    fs::{memfd_create, openat, statx, AtFlags, MemfdFlags, Mode, OFlags, StatxFlags, CWD},
    net::{RecvFlags, SendFlags, SocketAddrUnix, SocketFlags},
    process::{pidfd_open, pidfd_send_signal, PidfdFlags, Signal},
    stdio::stdin,
};
use serde::{ser::SerializeSeq, Deserialize, Serialize, Serializer};
use thiserror::Error;

/// The Ringboard (clipboard history) CLI.
///
/// Ringboard uses a client-server architecture, wherein the server has
/// exclusive write access to the clipboard database and clients must ask the
/// server to perform the modifications they need. This CLI is a non-interactive
/// client and debugging tool.
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
    #[command(aliases = ["at"])]
    Get(EntryAction),

    /// Add an entry to the database.
    ///
    /// The ID of the newly added entry will be returned.
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
    Swap(Swap),

    /// Delete an entry from the database.
    #[command(aliases = ["d", "delete", "destroy"])]
    Remove(EntryAction),

    /// Wipe the entire database.
    ///
    /// WARNING: this operation is irreversible. ALL DATA WILL BE LOST.
    #[command(alias = "nuke")]
    Wipe,

    /// Reload configuration files on the server.
    ReloadSettings(ReloadSettings),

    /// Migrate from other clipboard managers to Ringboard.
    Migrate(Migrate),

    /// Run garbage collection on the database.
    ///
    /// Returns the amount of freed space.
    #[command(aliases = ["gc", "clean"])]
    GarbageCollect,

    /// Debugging tools for developers.
    #[command(alias = "dev")]
    #[command(subcommand)]
    Debug(Dev),
}

#[derive(Subcommand, Debug)]
enum Dev {
    /// Print statistics about the Ringboard database.
    #[command(alias = "nerd")]
    Stats,

    /// Dump the contents of the database for analysis.
    #[command(alias = "export")]
    Dump(Dump),

    /// Generate a pseudo-random database for testing and performance tuning
    /// purposes.
    Generate(Generate),

    /// Spam the server with random commands.
    Fuzz(Fuzz),
}

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
struct Add {
    /// A file containing the data to be added to the entry.
    ///
    /// A value of `-` may be supplied to indicate that data should be read from
    /// STDIN.
    #[arg(required = true)]
    #[arg(value_hint = ValueHint::FilePath)]
    data_file: PathBuf,

    /// The target ring.
    #[clap(short, long, alias = "ring")]
    #[clap(default_value = "main")]
    target: CliRingKind,

    /// The entry mime type.
    #[clap(short, long)]
    #[clap(default_value = "")]
    mime_type: MimeType,
}

#[derive(ValueEnum, Copy, Clone, Debug)]
pub enum CliRingKind {
    Favorites,
    Main,
}

impl From<CliRingKind> for RingKind {
    fn from(value: CliRingKind) -> Self {
        match value {
            CliRingKind::Favorites => Self::Favorites,
            CliRingKind::Main => Self::Main,
        }
    }
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
struct Swap {
    /// The first entry ID.
    #[arg(required = true)]
    id1: u64,

    /// The second entry ID.
    #[arg(required = true)]
    id2: u64,
}

#[derive(Args, Debug)]
struct ReloadSettings {
    /// Use this configuration file instead of the default one.
    #[arg(short, long)]
    #[arg(value_hint = ValueHint::FilePath)]
    config: Option<PathBuf>,
}

#[derive(Args, Debug)]
#[command(arg_required_else_help = true)]
struct Migrate {
    /// The existing clipboard to migrate from.
    #[arg(required = true)]
    #[arg(requires_if("json", "database"))]
    from: MigrateFromClipboard,

    /// The existing clipboard's database location.
    ///
    /// This will be automatically inferred by default.
    #[clap(value_hint = ValueHint::AnyPath)]
    database: Option<PathBuf>,
}

#[derive(ValueEnum, Copy, Clone, Debug)]
enum MigrateFromClipboard {
    /// [Gnome Clipboard History](https://extensions.gnome.org/extension/4839/clipboard-history/)
    #[value(alias = "gch")]
    GnomeClipboardHistory,

    /// [Clipboard Indicator](https://extensions.gnome.org/extension/779/clipboard-indicator/)
    #[value(alias = "ci")]
    ClipboardIndicator,

    /// A sequence of JSON objects in the same format as the dump command.
    ///
    /// Note that the IDs are ignored and may be omitted.
    Json,
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
}

#[derive(Args, Debug)]
struct Dump {
    /// Instead of dumping the existing database contents, watch for new entries
    /// as they come in.
    #[arg(short, long)]
    watch: bool,
}

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] ringboard_core::Error),
    #[error("{0}")]
    Sdk(#[from] ringboard_sdk::ClientError),
    #[error("Failed to delete or copy files.")]
    Fuc(#[from] fuc_engine::Error),
    #[error("Id not found.")]
    IdNotFound(#[from] IdNotFoundError),
    #[error(
        "Database not found. Make sure to run the ringboard server or fix the XDG_DATA_HOME path."
    )]
    DatabaseNotFound(PathBuf),
    #[error("JSON (de)serialization failed.")]
    SerdeJson(#[from] serde_json::Error),
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
            CliError::Core(e) | CliError::Sdk(ringboard_sdk::ClientError::Core(e)) => {
                use ringboard_core::Error;
                match e {
                    Error::Io { error, context } => Report::new(error)
                        .attach_printable(context)
                        .change_context(wrapper),
                    Error::NotARingboard { file: _ } => Report::new(wrapper),
                    Error::InvalidPidError { error, context } => Report::new(error)
                        .attach_printable(context)
                        .change_context(wrapper),
                }
            }
            CliError::Fuc(fuc_engine::Error::Io { error, context }) => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            CliError::Sdk(ringboard_sdk::ClientError::InvalidResponse { context }) => {
                Report::new(wrapper).attach_printable(context)
            }
            CliError::Sdk(ringboard_sdk::ClientError::VersionMismatch { actual: _ }) => {
                Report::new(wrapper)
            }
            CliError::IdNotFound(IdNotFoundError::Ring(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown ring: {id}"))
            }
            CliError::IdNotFound(IdNotFoundError::Entry(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown entry: {id}"))
            }
            CliError::DatabaseNotFound(db) => {
                Report::new(wrapper).attach_printable(format!("Path: {:?}", db.display()))
            }
            CliError::Fuc(e) => Report::new(e).change_context(wrapper),
            CliError::SerdeJson(e) => Report::new(e).change_context(wrapper),
        }
    })
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
        Cmd::Add(data) => add(connect_to_server(&server_addr)?, &server_addr, data),
        Cmd::Favorite(data) => move_to_front(
            connect_to_server(&server_addr)?,
            &server_addr,
            data,
            Some(RingKind::Favorites),
        ),
        Cmd::Unfavorite(data) => move_to_front(
            connect_to_server(&server_addr)?,
            &server_addr,
            data,
            Some(RingKind::Main),
        ),
        Cmd::MoveToFront(data) => {
            move_to_front(connect_to_server(&server_addr)?, &server_addr, data, None)
        }
        Cmd::Swap(data) => swap(connect_to_server(&server_addr)?, &server_addr, data),
        Cmd::Remove(data) => remove(connect_to_server(&server_addr)?, &server_addr, data),
        Cmd::Wipe => wipe(),
        Cmd::ReloadSettings(data) => {
            reload_settings(connect_to_server(&server_addr)?, &server_addr, data)
        }
        Cmd::GarbageCollect => garbage_collect(connect_to_server(&server_addr)?, &server_addr),
        Cmd::Migrate(data) => migrate(connect_to_server(&server_addr)?, &server_addr, data),
        Cmd::Debug(Dev::Stats) => stats(),
        Cmd::Debug(Dev::Dump(Dump { watch: false })) => dump(),
        Cmd::Debug(Dev::Dump(Dump { watch: true })) => watch(),
        Cmd::Debug(Dev::Generate(data)) => {
            generate(connect_to_server(&server_addr)?, &server_addr, data)
        }
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

fn add(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    Add {
        data_file,
        target,
        mime_type,
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

        ringboard_sdk::add(
            server,
            addr,
            target.into(),
            mime_type,
            file.as_ref().map_or(stdin(), |file| file.as_fd()),
        )?
    };

    println!("Entry added: {id}");

    Ok(())
}

fn move_to_front(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    EntryAction { id }: EntryAction,
    to: Option<RingKind>,
) -> Result<(), CliError> {
    match ringboard_sdk::move_to_front(server, addr, id, to)? {
        MoveToFrontResponse::Success { id } => {
            println!("Entry moved: {id}");
        }
        MoveToFrontResponse::Error(e) => {
            return Err(CliError::IdNotFound(e));
        }
    }

    Ok(())
}

fn swap(server: OwnedFd, addr: &SocketAddrUnix, Swap { id1, id2 }: Swap) -> Result<(), CliError> {
    let SwapResponse { error1, error2 } = ringboard_sdk::swap(server, addr, id1, id2)?;
    if let Some(e) = error1 {
        return Err(CliError::IdNotFound(e));
    } else if let Some(e) = error2 {
        return Err(CliError::IdNotFound(e));
    }
    println!("Done.");

    Ok(())
}

fn remove(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    EntryAction { id }: EntryAction,
) -> Result<(), CliError> {
    let RemoveResponse { error } = ringboard_sdk::remove(server, addr, id)?;
    if let Some(e) = error {
        return Err(CliError::IdNotFound(e));
    }
    println!("Done.");

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

    let data_dir = data_dir();
    let mut tmp_data_dir = data_dir.with_extension("tmp");
    match fs::rename(&data_dir, &tmp_data_dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            println!("Nothing to delete");
            return Ok(());
        }
        r => r,
    }
    .map_io_err(|| format!("Failed to rename dir: {data_dir:?} -> {tmp_data_dir:?}"))?;

    tmp_data_dir.push("server.lock");
    let running_server = read_server_pid(CWD, &tmp_data_dir).ok().flatten();
    tmp_data_dir.pop();

    if let Some(pid) = running_server {
        let fd = pidfd_open(pid, PidfdFlags::empty())
            .map_io_err(|| format!("Failed to get FD for server: {pid:?}"))?;
        pidfd_send_signal(&fd, Signal::Quit)
            .map_io_err(|| format!("Failed to send shut down signal to server: {pid:?}"))?;

        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        poll(&mut fds, -1).map_io_err(|| format!("Failed to wait for server exit: {pid:?}"))?;
        if !fds[0].revents().contains(PollFlags::IN) {
            return Err(CliError::Core(ringboard_core::Error::Io {
                error: io::Error::new(ErrorKind::InvalidInput, "Bad poll response."),
                context: "Failed to receive server exit response.".into(),
            }));
        }
    }

    fuc_engine::remove_dir_all(tmp_data_dir)?;
    println!("Done.");

    Ok(())
}

fn reload_settings(
    _server: OwnedFd,
    _addr: &SocketAddrUnix,
    ReloadSettings { .. }: ReloadSettings,
) -> Result<(), CliError> {
    // TODO send config as ancillary data
    // TODO make config not an option by computing its default location at runtime
    // (if possible)
    todo!()
}

fn garbage_collect(server: OwnedFd, addr: &SocketAddrUnix) -> Result<(), CliError> {
    let GarbageCollectResponse { bytes_freed } = ringboard_sdk::garbage_collect(server, addr)?;
    println!("{bytes_freed} bytes of garbage freed.");
    Ok(())
}

fn migrate(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    Migrate { from, database }: Migrate,
) -> Result<(), CliError> {
    match from {
        MigrateFromClipboard::GnomeClipboardHistory => migrate_from_gch(server, addr, database),
        MigrateFromClipboard::ClipboardIndicator => todo!(),
        MigrateFromClipboard::Json => {
            migrate_from_ringboard_export(server, addr, database.unwrap())
        }
    }
}

#[allow(clippy::too_many_lines)]
fn migrate_from_gch(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    database: Option<PathBuf>,
) -> Result<(), CliError> {
    const OP_TYPE_SAVE_TEXT: u8 = 1;
    const OP_TYPE_DELETE_TEXT: u8 = 2;
    const OP_TYPE_FAVORITE_ITEM: u8 = 3;
    const OP_TYPE_UNFAVORITE_ITEM: u8 = 4;
    const OP_TYPE_MOVE_ITEM_TO_END: u8 = 5;

    fn generate_entry_file(database: impl AsFd, start: u64, len: usize) -> Result<File, CliError> {
        let file = openat(CWD, c".", OFlags::RDWR | OFlags::TMPFILE, Mode::empty())
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
            unsafe { Mmap::map(&file) }
                .map_io_err(|| format!("Failed to mmap file: {database:?}"))?,
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
                    drain_add_requests(&server, true, Some(&mut translation), &mut pending_adds)?;
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
                return Err(CliError::IdNotFound($e));
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

                pipeline_add_request(
                    &server,
                    addr,
                    data,
                    MimeType::new(),
                    Some(&mut translation),
                    &mut pending_adds,
                )?;
            }
            OP_TYPE_DELETE_TEXT => {
                if let RemoveResponse { error: Some(e) } =
                    ringboard_sdk::remove(&server, addr, get_translation!())?
                {
                    api_error!(e);
                }
                i += 4;
            }
            OP_TYPE_FAVORITE_ITEM | OP_TYPE_UNFAVORITE_ITEM | OP_TYPE_MOVE_ITEM_TO_END => {
                match ringboard_sdk::move_to_front(
                    &server,
                    addr,
                    get_translation!(),
                    match op {
                        OP_TYPE_FAVORITE_ITEM => Some(RingKind::Favorites),
                        OP_TYPE_UNFAVORITE_ITEM => Some(RingKind::Main),
                        OP_TYPE_MOVE_ITEM_TO_END => None,
                        _ => unreachable!(),
                    },
                )? {
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

    drain_add_requests(server, true, None, &mut pending_adds)
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn stats() -> Result<(), CliError> {
    #[derive(Default)]
    struct RingStats<'a> {
        capacity: u32,
        bucketed_entry_count: u32,
        file_entry_count: u32,

        entries: BTreeSet<Cow<'a, [u8]>>,
        num_duplicates: u32,
    }

    impl Debug for RingStats<'_> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            let Self {
                capacity,
                bucketed_entry_count,
                file_entry_count,
                entries: _,
                num_duplicates,
            } = self;
            f.debug_struct("RingStats")
                .field("capacity", capacity)
                .field("bucketed_entry_count", bucketed_entry_count)
                .field("file_entry_count", file_entry_count)
                .field("num_duplicates", num_duplicates)
                .finish()
        }
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
        owned_bytes: usize,
        allocated_bytes: u64,
        mime_types: BTreeMap<MimeType, u32>,
    }

    #[derive(Default, Debug)]
    struct Stats<'a> {
        rings: HashMap<RingKind, RingStats<'a>, BuildHasherDefault<FxHasher>>,
        buckets: [BucketStats; 11],
        direct_files: DirectFileStats,
    }

    impl Display for Stats<'_> {
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
                                capacity,
                                bucketed_entry_count,
                                file_entry_count,
                                ref entries,
                                num_duplicates: _,
                            },
                        ) in &self.rings
                        {
                            rings.key(kind).value_with(|f| {
                                let num_entries = bucketed_entry_count + file_entry_count;
                                let owned_bytes = entries.iter().map(|b| b.len()).sum::<usize>();
                                let mut s = f.debug_struct("Ring");
                                s.field("num_entries", &num_entries)
                                    .field("uninitialized_entry_count", &(capacity - num_entries))
                                    .field("owned_bytes", &owned_bytes)
                                    .field(
                                        "mean_entry_size",
                                        &(owned_bytes as f64 / f64::from(num_entries)),
                                    );
                                if !entries.is_empty() {
                                    s.field(
                                        "min_entry_size",
                                        &entries.iter().map(|b| b.len()).min().unwrap(),
                                    )
                                    .field(
                                        "max_entry_size",
                                        &entries.iter().map(|b| b.len()).max().unwrap(),
                                    );
                                }
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
                                &((allocated_bytes - u64::try_from(owned_bytes).unwrap()) as f64
                                    / allocated_bytes as f64),
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
                owned_bytes,
                allocated_bytes,
                mime_types,
            },
    } = &mut stats;

    let (database, reader) = open_db()?;

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
        *num_slots =
            u32::try_from(mem.len() / usize::try_from(bucket_to_length(i)).unwrap()).unwrap();
    }

    for ring_reader in [database.main(), database.favorites()] {
        let mut ring_stats = RingStats::default();
        let RingStats {
            capacity,
            bucketed_entry_count,
            file_entry_count,
            entries,
            num_duplicates,
        } = &mut ring_stats;
        *capacity = ring_reader.ring().capacity();
        let kind = ring_reader.kind();

        for entry in ring_reader {
            match entry.kind() {
                Kind::Bucket(entry) => {
                    *bucketed_entry_count += 1;

                    let bucket = size_to_bucket(entry.size());
                    let BucketStats {
                        size_class: _,
                        num_slots: _,
                        used_slots,
                        owned_bytes,
                    } = &mut buckets[bucket];
                    *used_slots += 1;
                    *owned_bytes += u64::from(entry.size());
                }
                Kind::File => {
                    *file_entry_count += 1;
                }
            }

            // TODO replace with hashing so we can mutably borrow
            let data = entry.to_slice_raw(&reader)?.unwrap();
            if let Some(fd) = data.backing_file() {
                *owned_bytes += data.len();
                *mime_types.entry(data.mime_type()?).or_default() += 1;
                *allocated_bytes += statx(fd, c"", AtFlags::EMPTY_PATH, StatxFlags::BLOCKS)
                    .map_io_err(|| format!("Failed to statx entry: {entry:?}"))?
                    .stx_blocks
                    * 512;
            }
            if !entries.insert(data.into_inner()) {
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
            data: if let Ok(data) = str::from_utf8(&loaded) {
                ExportData::Human(data.into())
            } else {
                ExportData::Bytes(loaded.into_inner())
            },
            mime_type,
        })?;
    }

    SerializeSeq::end(seq)?;
    Ok(())
}

fn migrate_from_ringboard_export(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    dump_file: PathBuf,
) -> Result<(), CliError> {
    fn generate_entry_file(data: &[u8]) -> Result<File, CliError> {
        let file = File::from(
            openat(CWD, c".", OFlags::RDWR | OFlags::TMPFILE, Mode::empty())
                .map_io_err(|| "Failed to create data entry file.")?,
        );

        file.write_all_at(data, 0)
            .map_io_err(|| "Failed to copy data to entry file.")?;

        Ok(file)
    }

    let mut pending_adds = 0;
    let mut process = |ExportEntry {
                           id: _,
                           data,
                           mime_type,
                       }|
     -> Result<(), CliError> {
        let data = generate_entry_file(match &data {
            ExportData::Human(str) => str.as_bytes(),
            ExportData::Bytes(bytes) => bytes,
        })?;

        pipeline_add_request(&server, addr, data, mime_type, None, &mut pending_adds)
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
        let dump = unsafe { Mmap::map(&dump) }
            .map_io_err(|| format!("Failed to mmap file: {dump_file:?}"))?;
        drop(dump_file);

        let iter = serde_json::Deserializer::from_slice(&dump).into_iter::<ExportEntry>();
        for result in iter {
            process(result?)?;
        }
    };

    drain_add_requests(server, true, None, &mut pending_adds)
}

fn watch() -> Result<(), CliError> {
    todo!()
}

fn generate(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    Generate {
        num_entries,
        mean_size,
        cv_size,
    }: Generate,
) -> Result<(), CliError> {
    let distr = LogNormal::from_mean_cv(f64::from(mean_size), f64::from(cv_size)).unwrap();
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(u64::from(num_entries));
    let mut pending_adds = 0;

    for _ in 0..num_entries {
        let data = generate_random_entry_file(&mut rng, distr)?;
        pipeline_add_request(
            &server,
            addr,
            data,
            MimeType::new(),
            None,
            &mut pending_adds,
        )?;
    }

    drain_add_requests(server, true, None, &mut pending_adds)
}

#[allow(clippy::too_many_lines)]
fn fuzz(
    addr: &SocketAddrUnix,
    Fuzz {
        seed,
        mean_size,
        cv_size,
    }: Fuzz,
) -> Result<(), CliError> {
    struct FuzzRingKind(RingKind);

    impl Distribution<FuzzRingKind> for Standard {
        fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> FuzzRingKind {
            match rng.gen_range(0..=2) {
                0 | 1 => FuzzRingKind(RingKind::Main),
                2 => FuzzRingKind(RingKind::Favorites),
                _ => unreachable!(),
            }
        }
    }

    let distr = WeightedAliasIndex::new(vec![550u32, 450, 40000, 20000, 20000, 20000, 1]).unwrap();
    let entry_size_distr =
        LogNormal::from_mean_cv(f64::from(mean_size), f64::from(cv_size)).unwrap();
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut buf = String::with_capacity(MimeType::new_const().capacity());

    let mut clients = Vec::with_capacity(32);
    let mut max_id_seen = 0;
    let mut data = HashMap::new();

    let (mut database, mut reader) = open_db()?;
    let mut out = io::stdout().lock();
    loop {
        match distr.sample(&mut rng) {
            0 => {
                writeln!(out, "Connecting.").unwrap();
                if let Ok(client) = if clients.len() == 32 {
                    connect_to_server_with(addr, SocketFlags::NONBLOCK)
                } else {
                    connect_to_server(addr)
                } {
                    clients.push(client);
                }
            }
            1 => {
                writeln!(out, "Closing.").unwrap();
                if !clients.is_empty() {
                    clients.swap_remove(rng.gen_range(0..clients.len()));
                }
            }
            action @ 2..=5 => {
                let server = if clients.is_empty() {
                    clients.push(connect_to_server(addr)?);
                    &clients[0]
                } else {
                    &clients[rng.gen_range(0..clients.len())]
                };

                match action {
                    2 => {
                        writeln!(out, "Adding.").unwrap();
                        let mime_type = if rng.gen() {
                            MimeType::new()
                        } else {
                            let len = rng.gen_range(1..=MimeType::new_const().capacity());
                            Alphanumeric.append_string(&mut rng, &mut buf, len);

                            let mime = MimeType::from(&buf).unwrap();
                            buf.clear();
                            mime
                        };

                        let file = generate_random_entry_file(&mut rng, entry_size_distr)?;
                        let AddResponse::Success { id } = ringboard_sdk::add(
                            server,
                            addr,
                            rng.gen::<FuzzRingKind>().0,
                            mime_type,
                            &file,
                        )?;
                        data.insert(
                            id,
                            unsafe { Mmap::map(&file) }
                                .map_io_err(|| format!("Failed to mmap file: {file:?}"))?,
                        );
                        max_id_seen = max_id_seen.max(id);
                    }
                    3 => {
                        writeln!(out, "Moving.").unwrap();
                        let move_id = rng.gen_range(0..=max_id_seen);
                        match ringboard_sdk::move_to_front(
                            server,
                            addr,
                            move_id,
                            rng.gen::<Option<FuzzRingKind>>().map(|r| r.0),
                        )? {
                            MoveToFrontResponse::Success { id } => {
                                let file = data.remove(&move_id).unwrap();
                                data.insert(id, file);
                                max_id_seen = max_id_seen.max(id);
                            }
                            MoveToFrontResponse::Error(_) => {
                                assert!(!data.contains_key(&move_id));
                            }
                        }
                    }
                    4 => {
                        writeln!(out, "Swapping.").unwrap();
                        let idx1 = rng.gen_range(0..=max_id_seen);
                        let idx2 = rng.gen_range(0..=max_id_seen);
                        match ringboard_sdk::swap(server, addr, idx1, idx2)? {
                            SwapResponse {
                                error1: None,
                                error2: None,
                            } => {
                                let file1 = data.remove(&idx1);
                                let file2 = data.remove(&idx2);
                                assert!(file1.is_some() || file2.is_some());

                                if let Some(file2) = file2 {
                                    data.insert(idx1, file2);
                                }
                                if let Some(file1) = file1 {
                                    data.insert(idx2, file1);
                                }
                            }
                            SwapResponse { error1, error2 } => {
                                if error1.is_some() {
                                    assert!(!data.contains_key(&idx1));
                                }
                                if error2.is_some() {
                                    assert!(!data.contains_key(&idx2));
                                }
                            }
                        }
                    }
                    5 => {
                        writeln!(out, "Removing.").unwrap();
                        let index = rng.gen_range(0..=max_id_seen);
                        match ringboard_sdk::remove(server, addr, index)? {
                            RemoveResponse { error: None } => {
                                data.remove(&index);
                            }
                            RemoveResponse { error: Some(_) } => {
                                assert!(!data.contains_key(&index));
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }
            6 => {
                writeln!(
                    out,
                    "Validating database integrity on {} entries.",
                    data.len()
                )
                .unwrap();

                for (&id, a) in &data {
                    let entry = unsafe { database.get(id) }?;
                    let b = match entry.kind() {
                        Kind::Bucket(_) => &*entry.to_slice(&mut reader)?,
                        Kind::File => {
                            let db_file = entry.to_file(&mut reader)?;
                            &*unsafe { Mmap::map(&*db_file) }
                                .map_io_err(|| format!("Failed to mmap file: {db_file:?}"))?
                        }
                    };

                    assert_eq!(**a, *b);
                }
            }
            _ => unreachable!(),
        }
    }
}

fn pipeline_add_request(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    data: impl AsFd,
    mime_type: MimeType,
    mut translation: Option<&mut Vec<u64>>,
    pending_adds: &mut u32,
) -> Result<(), CliError> {
    let mut retry = false;
    loop {
        match ringboard_sdk::add_send(
            &server,
            addr,
            RingKind::Main,
            mime_type,
            &data,
            if *pending_adds == 0 {
                SendFlags::empty()
            } else {
                SendFlags::DONTWAIT
            },
        ) {
            Err(ringboard_sdk::ClientError::Core(ringboard_core::Error::Io {
                error: e, ..
            })) if e.kind() == ErrorKind::WouldBlock => {
                debug_assert!(*pending_adds > 0);
                drain_add_requests(&server, retry, translation.as_deref_mut(), pending_adds)?;
                retry = true;
            }
            r => {
                r?;
                *pending_adds += 1;
                break;
            }
        };
    }
    Ok(())
}

fn drain_add_requests(
    server: impl AsFd,
    all: bool,
    mut translation: Option<&mut Vec<u64>>,
    pending_adds: &mut u32,
) -> Result<(), CliError> {
    while *pending_adds > 0 {
        let AddResponse::Success { id } = match unsafe {
            ringboard_sdk::add_recv(
                &server,
                if all {
                    RecvFlags::empty()
                } else {
                    RecvFlags::DONTWAIT
                },
            )
        } {
            Err(ringboard_sdk::ClientError::Core(ringboard_core::Error::Io {
                error: e, ..
            })) if e.kind() == ErrorKind::WouldBlock => {
                debug_assert!(!all);
                break;
            }
            r => r?,
        };

        *pending_adds -= 1;
        if let Some(translation) = translation.as_deref_mut() {
            translation.push(id);
        }
    }
    Ok(())
}

fn generate_random_entry_file(
    rng: &mut (impl RngCore + 'static),
    len_distr: LogNormal<f64>,
) -> Result<File, CliError> {
    let mut file = File::from(
        memfd_create("ringboard_gen", MemfdFlags::empty())
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

    Ok(file)
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
