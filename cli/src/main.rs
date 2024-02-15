use std::{
    borrow::Cow,
    fs::File,
    io::{IoSlice, IoSliceMut, Write},
    mem::size_of,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    ptr, slice, str,
    str::FromStr,
};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, ValueHint};
use clap_num::si_number;
use clipboard_history_core::{dirs::socket_file, protocol, protocol::Request, Error, IoErr};
use error_stack::Report;
use rustix::{
    net::{
        connect_unix, recvmsg, sendmsg_unix, socket, AddressFamily, RecvAncillaryBuffer, RecvFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketType,
    },
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
struct Add {
    /// A file containing the data to be added to the entry.
    ///
    /// A value of `-` may be supplied to indicate that data should be read from
    /// STDIN.
    #[arg(required = true)]
    #[arg(value_hint = ValueHint::FilePath)]
    data_file: PathBuf,
}

#[derive(Args, Debug)]
struct EntryAction {
    /// The entry ID.
    #[arg(required = true)]
    id: u32,
}

#[derive(Args, Debug)]
struct Swap {
    /// The first entry ID.
    #[arg(required = true)]
    id1: u32,

    /// The second entry ID.
    #[arg(required = true)]
    id2: u32,
}

#[derive(Args, Debug)]
struct ReloadSettings {
    /// Use this configuration file instead of the default one.
    #[arg(short, long)]
    #[arg(value_hint = ValueHint::FilePath)]
    config: Option<PathBuf>,
}

#[derive(Args, Debug)]
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
    Core(#[from] Error),
    #[error(
        "Protocol version mismatch: expected {} but got {actual:?}",
        protocol::VERSION
    )]
    VersionMismatch { actual: Option<u8> },
    #[error("The server returned an invalid entry ID.")]
    InvalidEntryId { context: Cow<'static, str> },
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
            CliError::Core(Error::Io { error, context }) => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            CliError::Core(Error::NotARingboard { file: _ })
            | CliError::VersionMismatch { actual: _ } => Report::new(wrapper),
            CliError::Core(Error::InvalidPidError { error, context }) => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            CliError::InvalidEntryId { context } => Report::new(wrapper).attach_printable(context),
        }
    })
}

fn run() -> Result<(), CliError> {
    let Cli { cmd, help: _ } = Cli::parse();

    let socket_file = socket_file();
    let server_addr = SocketAddrUnix::new(&socket_file)
        .map_io_err(|| format!("Failed to make socket address: {socket_file:?}"))?;
    match cmd {
        Cmd::Add(data) => add(
            data,
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
        )?,
        Cmd::Favorite(_) => {}
        Cmd::Unfavorite(_) => {}
        Cmd::MoveToFront(_) => {}
        Cmd::Swap(_) => {}
        Cmd::Remove(_) => {}
        Cmd::Wipe => {}
        Cmd::ReloadSettings(_) => {}
        Cmd::Migrate(_) => {}
        Cmd::GarbageCollect => {}
        Cmd::Debug(_) => {}
    }
    todo!()
}

fn connect_to_server(addr: &SocketAddrUnix, socket_file: &Path) -> Result<OwnedFd, CliError> {
    let socket = socket(AddressFamily::UNIX, SocketType::SEQPACKET, None)
        .map_io_err(|| format!("Failed to create socket: {socket_file:?}"))?;
    connect_unix(&socket, addr)
        .map_io_err(|| format!("Failed to connect to server: {socket_file:?}"))?;

    {
        sendmsg_unix(
            &socket,
            addr,
            &[IoSlice::new(&[protocol::VERSION])],
            &mut SendAncillaryBuffer::default(),
            SendFlags::empty(),
        )
        .map_io_err(|| "Failed to send version.")?;

        let mut version = 0;
        let result = recvmsg(
            &socket,
            &mut [IoSliceMut::new(slice::from_mut(&mut version))],
            &mut RecvAncillaryBuffer::default(),
            RecvFlags::TRUNC,
        )
        .map_io_err(|| "Failed to receive version.")?;

        if result.bytes != 1 {
            return Err(CliError::VersionMismatch { actual: None });
        }
        if version != protocol::VERSION {
            return Err(CliError::VersionMismatch {
                actual: Some(version),
            });
        }
    }

    Ok(socket)
}

fn add(Add { data_file }: Add, server: OwnedFd, addr: SocketAddrUnix) -> Result<(), CliError> {
    {
        let mut space = [0; rustix::cmsg_space!(ScmRights(1))];
        let mut buf = SendAncillaryBuffer::new(&mut space);
        let file = if data_file == Path::new("-") {
            None
        } else {
            Some(
                File::open(&data_file)
                    .map_io_err(|| format!("Failed to open file: {data_file:?}"))?,
            )
        };
        let fds = [file.as_ref().map(|file| file.as_fd()).unwrap_or(stdin())];
        debug_assert!(buf.push(SendAncillaryMessage::ScmRights(&fds)));

        sendmsg_unix(
            &server,
            &addr,
            &[IoSlice::new(Request::Add.as_bytes())],
            &mut buf,
            SendFlags::empty(),
        )
        .map_io_err(|| "Failed to send add request.")?;
    }

    let mut buf = [0u8; 4];
    let result = recvmsg(
        &server,
        &mut [IoSliceMut::new(buf.as_mut_slice())],
        &mut RecvAncillaryBuffer::default(),
        RecvFlags::TRUNC,
    )
    .map_io_err(|| "Failed to receive add response.")?;
    if result.bytes != buf.len() {
        return Err(CliError::InvalidEntryId {
            context: "Bad add response.".into(),
        });
    }

    println!("Entry added: {}", u32::from_le_bytes(buf));

    Ok(())
}

trait AsBytes<T> {
    fn as_bytes(&self) -> &[u8];
}

impl<T> AsBytes<T> for T {
    fn as_bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(ptr::from_ref::<T>(self).cast(), size_of::<T>()) }
    }
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
