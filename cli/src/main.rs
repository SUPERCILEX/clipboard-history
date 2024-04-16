use std::{
    fs,
    fs::File,
    io,
    io::ErrorKind,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    str,
};

use ask::Answer;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, ValueHint};
use clap_num::si_number;
use error_stack::Report;
use memmap2::Mmap;
use ringboard_core::{
    copy_file_range_all,
    dirs::{data_dir, socket_file},
    protocol::{
        AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, RemoveResponse, RingKind,
        SwapResponse,
    },
    read_server_pid, IoErr,
};
use ringboard_sdk::{connect_to_server, garbage_collect};
use rustix::{
    event::{poll, PollFd, PollFlags},
    fs::{openat, Mode, OFlags, CWD},
    net::{RecvFlags, SendFlags, SocketAddrUnix},
    process::{pidfd_open, pidfd_send_signal, PidfdFlags, Signal},
    stdio::stdin,
};
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
    /// Add an entry to the database.
    ///
    /// The ID of the newly added entry will be returned.
    #[command(aliases = ["new", "create", "copy"])]
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
    #[command(aliases = ["delete", "destroy"])]
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
    #[clap(default_value = "text/plain")]
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
}

#[derive(Args, Debug)]
struct Generate {
    /// The number of random entries to generate.
    #[clap(short, long = "entries", alias = "num-entries")]
    #[clap(value_parser = si_number::< u32 >)]
    #[clap(default_value = "1_000_000")]
    num_entries: u32,

    /// The mean entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::< usize >)]
    #[clap(default_value = "128")]
    mean_size: usize,

    /// The standard deviation of the entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::< usize >)]
    #[clap(default_value = "100")]
    stddev_size: usize,
}

#[derive(Args, Debug)]
struct Fuzz {
    /// The number of random entries to generate.
    #[clap(short = 'c', long = "clients", alias = "num-clients")]
    #[clap(value_parser = si_number::< usize >)]
    #[clap(default_value = "3")]
    num_clients: usize,
}

#[derive(Args, Debug)]
struct Dump {
    /// Include the plain-text contents of each entry.
    #[arg(short, long)]
    contents: bool,

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
    Sdk(#[from] ringboard_sdk::Error),
    #[error("Failed to delete or copy files.")]
    Fuc(fuc_engine::Error),
    #[error("Id not found.")]
    IdNotFound(IdNotFoundError),
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
            CliError::Core(e) | CliError::Sdk(ringboard_sdk::Error::Core(e)) => {
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
            CliError::Sdk(ringboard_sdk::Error::InvalidResponse { context }) => {
                Report::new(wrapper).attach_printable(context)
            }
            CliError::Sdk(ringboard_sdk::Error::VersionMismatch { actual: _ }) => {
                Report::new(wrapper)
            }
            CliError::IdNotFound(IdNotFoundError::Ring(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown ring: {id}"))
            }
            CliError::IdNotFound(IdNotFoundError::Entry(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown entry: {id}"))
            }
            CliError::Fuc(e) => Report::new(e).change_context(wrapper),
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
            reload_settings(connect_to_server(&server_addr)?, server_addr, data)
        }
        Cmd::GarbageCollect => {
            garbage_collect(connect_to_server(&server_addr)?, &server_addr).map_err(CliError::from)
        }
        Cmd::Migrate(data) => migrate(connect_to_server(&server_addr)?, &server_addr, data),
        Cmd::Debug(Dev::Stats) => stats(),
        Cmd::Debug(Dev::Dump(data)) => dump(data),
        Cmd::Debug(Dev::Generate(data)) => {
            generate(connect_to_server(&server_addr)?, server_addr, data)
        }
        Cmd::Debug(Dev::Fuzz(data)) => fuzz(connect_to_server(&server_addr)?, server_addr, data),
    }
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

    fuc_engine::remove_dir_all(tmp_data_dir).map_err(CliError::Fuc)?;
    println!("Done.");

    Ok(())
}

fn reload_settings(
    server: OwnedFd,
    addr: SocketAddrUnix,
    ReloadSettings { config }: ReloadSettings,
) -> Result<(), CliError> {
    // TODO send config as ancillary data
    // TODO make config not an option by computing its default location at runtime
    // (if possible)
    todo!()
}

fn migrate(
    server: OwnedFd,
    addr: &SocketAddrUnix,
    Migrate { from, database }: Migrate,
) -> Result<(), CliError> {
    match from {
        MigrateFromClipboard::GnomeClipboardHistory => migrate_from_gch(server, addr, database),
        MigrateFromClipboard::ClipboardIndicator => todo!(),
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

    fn drain_add_requests(
        server: impl AsFd,
        all: bool,
        translation: &mut Vec<u64>,
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
                Err(ringboard_sdk::Error::Core(ringboard_core::Error::Io { error: e, .. }))
                    if e.kind() == ErrorKind::WouldBlock =>
                {
                    debug_assert!(!all);
                    break;
                }
                r => r?,
            };

            *pending_adds -= 1;
            translation.push(id);
        }
        Ok(())
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
        drain_add_requests(&server, false, &mut translation, &mut pending_adds)?;
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
                    drain_add_requests(&server, true, &mut translation, &mut pending_adds)?;
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

                loop {
                    match ringboard_sdk::add_send(
                        &server,
                        addr,
                        RingKind::Main,
                        MimeType::new(),
                        &data,
                        if pending_adds == 0 {
                            SendFlags::empty()
                        } else {
                            SendFlags::DONTWAIT
                        },
                    ) {
                        Err(ringboard_sdk::Error::Core(ringboard_core::Error::Io {
                            error: e,
                            ..
                        })) if e.kind() == ErrorKind::WouldBlock => {
                            debug_assert!(pending_adds > 0);
                            drain_add_requests(&server, true, &mut translation, &mut pending_adds)?;
                        }
                        r => {
                            r?;
                            pending_adds += 1;
                            break;
                        }
                    };
                }
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

    drain_add_requests(server, true, &mut translation, &mut pending_adds)
}

fn stats() -> Result<(), CliError> {
    todo!()
}

fn dump(Dump { contents, watch }: Dump) -> Result<(), CliError> {
    todo!()
}

fn generate(
    server: OwnedFd,
    addr: SocketAddrUnix,
    Generate {
        num_entries,
        mean_size,
        stddev_size,
    }: Generate,
) -> Result<(), CliError> {
    todo!()
}

fn fuzz(server: OwnedFd, addr: SocketAddrUnix, Fuzz { num_clients }: Fuzz) -> Result<(), CliError> {
    todo!()
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
