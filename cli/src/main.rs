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
use ringboard_core::{
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
    fs::CWD,
    net::SocketAddrUnix,
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
        Cmd::Migrate(data) => migrate(connect_to_server(&server_addr)?, server_addr, data),
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
    addr: SocketAddrUnix,
    Migrate { from }: Migrate,
) -> Result<(), CliError> {
    todo!()
}

fn stats() -> Result<(), CliError> {
    todo!()
}

fn dump(Dump { contents }: Dump) -> Result<(), CliError> {
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
