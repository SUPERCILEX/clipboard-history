use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, ValueHint};
use clap_num::si_number;

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
    #[clap(value_parser = si_number::<u32>)]
    #[clap(default_value = "1_000_000")]
    num_entries: u32,

    /// The mean entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::<usize>)]
    #[clap(default_value = "128")]
    mean_size: usize,

    /// The standard deviation of the entry size.
    #[clap(short, long)]
    #[clap(value_parser = si_number::<usize>)]
    #[clap(default_value = "100")]
    stddev_size: usize,
}

#[derive(Args, Debug)]
struct Fuzz {
    /// The number of random entries to generate.
    #[clap(short = 'c', long = "clients", alias = "num-clients")]
    #[clap(value_parser = si_number::<usize>)]
    #[clap(default_value = "3")]
    num_clients: usize,
}

#[derive(Args, Debug)]
struct Dump {
    /// Include the plain-text contents of each entry.
    #[arg(short, long)]
    contents: bool,
}

fn main() {
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
