use std::{
    borrow::Cow,
    fs::File,
    io::{IoSlice, IoSliceMut},
    mem,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    str,
};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, ValueHint};
use clap_num::si_number;
use clipboard_history_core::{
    dirs::socket_file,
    protocol,
    protocol::{
        AddResponse, IdNotFoundError, MimeType, MoveToFrontResponse, RemoveResponse, Request,
        RingKind, SwapResponse,
    },
    AsBytes, Error, IoErr,
};
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
            CliRingKind::Favorites => RingKind::Favorites,
            CliRingKind::Main => RingKind::Main,
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
    Core(#[from] Error),
    #[error(
        "Protocol version mismatch: expected {} but got {actual}",
        protocol::VERSION
    )]
    VersionMismatch { actual: u8 },
    #[error("The server returned an invalid response.")]
    InvalidResponse { context: Cow<'static, str> },
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
            CliError::Core(Error::Io { error, context }) => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            CliError::Core(Error::NotARingboard { file: _ })
            | CliError::VersionMismatch { actual: _ } => Report::new(wrapper),
            CliError::Core(Error::InvalidPidError { error, context }) => Report::new(error)
                .attach_printable(context)
                .change_context(wrapper),
            CliError::InvalidResponse { context } => Report::new(wrapper).attach_printable(context),
            CliError::IdNotFound(IdNotFoundError::Ring(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown ring: {id}"))
            }
            CliError::IdNotFound(IdNotFoundError::Entry(id)) => {
                Report::new(wrapper).attach_printable(format!("Unknown entry: {id}"))
            }
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
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
        ),
        Cmd::Favorite(data) => move_to_front(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
            Some(RingKind::Favorites),
        ),
        Cmd::Unfavorite(data) => move_to_front(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
            Some(RingKind::Main),
        ),
        Cmd::MoveToFront(data) => move_to_front(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
            None,
        ),
        Cmd::Swap(data) => swap(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
        ),
        Cmd::Remove(data) => remove(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
        ),
        Cmd::Wipe => wipe(),
        Cmd::ReloadSettings(data) => reload_settings(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
        ),
        Cmd::GarbageCollect => request(
            connect_to_server(&server_addr, &socket_file)?,
            &server_addr,
            Request::GarbageCollect,
        ),
        Cmd::Migrate(data) => migrate(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
        ),
        Cmd::Debug(Dev::Stats) => stats(),
        Cmd::Debug(Dev::Dump(data)) => dump(data),
        Cmd::Debug(Dev::Generate(data)) => generate(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
        ),
        Cmd::Debug(Dev::Fuzz(data)) => fuzz(
            connect_to_server(&server_addr, &socket_file)?,
            server_addr,
            data,
        ),
    }
}

fn request(server: impl AsFd, addr: &SocketAddrUnix, request: Request) -> Result<(), CliError> {
    request_with_ancillary(server, addr, request, &mut SendAncillaryBuffer::default())
}

fn request_with_fd(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    request: Request,
    fd: BorrowedFd,
) -> Result<(), CliError> {
    let mut space = [0; rustix::cmsg_space!(ScmRights(1))];
    let mut buf = SendAncillaryBuffer::new(&mut space);
    let fds = [fd];
    debug_assert!(buf.push(SendAncillaryMessage::ScmRights(&fds)));

    request_with_ancillary(server, addr, request, &mut buf)
}

fn request_with_ancillary(
    server: impl AsFd,
    addr: &SocketAddrUnix,
    request: Request,
    ancillary: &mut SendAncillaryBuffer,
) -> Result<(), CliError> {
    sendmsg_unix(
        server,
        addr,
        &[IoSlice::new(request.as_bytes())],
        ancillary,
        SendFlags::empty(),
    )
    .map_io_err(|| format!("Failed to send request: {request:?}"))?;
    Ok(())
}

unsafe fn response<T: Copy, const N: usize>(server: impl AsFd) -> Result<T, CliError> {
    let type_name = || {
        let name = std::any::type_name::<T>();
        if let Some((_, name)) = name.rsplit_once(':') {
            name
        } else {
            name
        }
    };

    let mut buf = [0u8; N];
    let result = recvmsg(
        server,
        &mut [IoSliceMut::new(buf.as_mut_slice())],
        &mut RecvAncillaryBuffer::default(),
        RecvFlags::TRUNC,
    )
    .map_io_err(|| format!("Failed to receive {}.", type_name()))?;
    if result.bytes != mem::size_of::<T>() {
        return Err(CliError::InvalidResponse {
            context: format!("Bad {}.", type_name()).into(),
        });
    }
    Ok(unsafe { *buf.as_ptr().cast::<T>() })
}

macro_rules! response {
    ($t:ty) => {
        response::<$t, { mem::size_of::<$t>() }>
    };
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

        let version = unsafe { response!(u8)(&socket) }?;
        if version != protocol::VERSION {
            return Err(CliError::VersionMismatch { actual: version });
        }
    }

    Ok(socket)
}

fn add(
    server: OwnedFd,
    addr: SocketAddrUnix,
    Add {
        data_file,
        target,
        mime_type,
    }: Add,
) -> Result<(), CliError> {
    {
        let file = if data_file == Path::new("-") {
            None
        } else {
            Some(
                File::open(&data_file)
                    .map_io_err(|| format!("Failed to open file: {data_file:?}"))?,
            )
        };

        request_with_fd(
            &server,
            &addr,
            Request::Add {
                to: target.into(),
                mime_type,
            },
            file.as_ref().map(|file| file.as_fd()).unwrap_or(stdin()),
        )?;
    }

    let AddResponse::Success { id } = unsafe { response!(AddResponse)(&server) }?;
    println!("Entry added: {id}");

    Ok(())
}

fn move_to_front(
    server: OwnedFd,
    addr: SocketAddrUnix,
    EntryAction { id }: EntryAction,
    to: Option<RingKind>,
) -> Result<(), CliError> {
    request(&server, &addr, Request::MoveToFront { id, to })?;

    match unsafe { response!(MoveToFrontResponse)(&server) }? {
        MoveToFrontResponse::Success { id } => {
            println!("Entry moved: {id}");
        }
        MoveToFrontResponse::Error(e) => {
            return Err(CliError::IdNotFound(e));
        }
    }

    Ok(())
}

fn swap(server: OwnedFd, addr: SocketAddrUnix, Swap { id1, id2 }: Swap) -> Result<(), CliError> {
    request(&server, &addr, Request::Swap { id1, id2 })?;

    let SwapResponse { error1, error2 } = unsafe { response!(SwapResponse)(&server) }?;
    if let Some(e) = error1 {
        return Err(CliError::IdNotFound(e));
    } else if let Some(e) = error2 {
        return Err(CliError::IdNotFound(e));
    } else {
        println!("Done.");
    }

    Ok(())
}

fn remove(
    server: OwnedFd,
    addr: SocketAddrUnix,
    EntryAction { id }: EntryAction,
) -> Result<(), CliError> {
    request(&server, &addr, Request::Remove { id })?;

    let RemoveResponse { error } = unsafe { response!(RemoveResponse)(&server) }?;
    if let Some(e) = error {
        return Err(CliError::IdNotFound(e));
    } else {
        println!("Done.");
    }

    Ok(())
}

fn wipe() -> Result<(), CliError> {
    // TODO move directory, shut down server if running, then delete
    // TODO server needs to only use paths on startup and use relative FDs
    // otherwise
    todo!()
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
