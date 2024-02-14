use std::{borrow::Cow, fs, num::NonZeroU32, path::PathBuf};

use clipboard_history_core::{
    dirs::{data_dir, socket_file},
    Error, IoErr,
};
use error_stack::Report;
use log::info;
use thiserror::Error;

use crate::{allocator::Allocator, startup::claim_server_ownership, views::PathView};

mod allocator;
mod reactor;
mod requests;
mod send_msg_bufs;
mod startup;
mod views;

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] Error),
    #[error("The server is already running (PID {pid})")]
    ServerAlreadyRunning { pid: NonZeroU32, lock_file: PathBuf },
    #[error("Failed to deserialize free lists.")]
    FreeListsDeserializeError {
        file: PathBuf,
        error: bitcode::Error,
    },
    #[error("Failed to serialize free lists.")]
    FreeListsSerializeError(bitcode::Error),
    #[error("Multiple errors occurred.")]
    Multiple(Vec<CliError>),
    #[error("Internal error")]
    Internal { context: Cow<'static, str> },
}

#[derive(Error, Debug)]
enum Wrapper {
    #[error("{0}")]
    W(String),
}

fn main() -> error_stack::Result<(), Wrapper> {
    #[cfg(not(debug_assertions))]
    error_stack::Report::install_debug_hook::<std::panic::Location>(|_, _| {});

    if cfg!(debug_assertions) {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    } else {
        env_logger::init();
    }

    run().map_err(into_report)
}

fn into_report(cli_err: CliError) -> Report<Wrapper> {
    let wrapper = Wrapper::W(cli_err.to_string());
    match cli_err {
        CliError::Core(Error::Io { error, context }) => Report::new(error)
            .attach_printable(context)
            .change_context(wrapper),
        CliError::Core(Error::NotARingboard { file: _ }) => Report::new(wrapper),
        CliError::Core(Error::InvalidPidError { error, context }) => Report::new(error)
            .attach_printable(context)
            .change_context(wrapper),
        CliError::ServerAlreadyRunning { pid: _, lock_file } => Report::new(wrapper)
            .attach_printable(
                "Unable to safely start server: please shut down the existing instance. If \
                 something has gone terribly wrong, please create an empty server lock file to \
                 initiate the recovery sequence on the next startup.",
            )
            .attach_printable(format!("Lock file: {lock_file:?}")),
        CliError::FreeListsDeserializeError { file, error } => Report::new(wrapper)
            .attach_printable(error)
            .attach_printable(format!("Free lists file: {file:?}")),
        CliError::FreeListsSerializeError(error) => Report::new(wrapper).attach_printable(error),
        CliError::Multiple(mut errs) => {
            let mut report = into_report(errs.pop().unwrap_or(CliError::Internal {
                context: "Multiple errors variant contained no errors".into(),
            }));
            report.extend(errs.into_iter().map(into_report));
            report
        }
        CliError::Internal { context } => Report::new(wrapper)
            .attach_printable(context)
            .attach_printable(
            "Please report this bug at https://github.com/SUPERCILEX/clipboard-history/issues/new",
        ),
    }
}

fn into_result(errs: Vec<CliError>) -> Result<(), CliError> {
    if errs.is_empty() {
        Ok(())
    } else {
        Err(CliError::Multiple(errs))
    }
}

fn run() -> Result<(), CliError> {
    let mut data_dir = data_dir();
    fs::create_dir_all(&data_dir)
        .map_io_err(|| format!("Failed to create data directory: {data_dir:?}"))?;
    let server_guard = {
        let lock = PathView::new(&mut data_dir, "server.lock");
        loop {
            if let Some(g) = claim_server_ownership(&lock)? {
                break g;
            }

            fs::remove_file(&lock)
                .map_io_err(|| format!("Failed to delete server lock: {lock:?}"))?;
        }
    };
    let socket_file = socket_file();
    info!("Acquired server lock.");

    let mut allocator = Allocator::open(data_dir)?;
    into_result(
        [
            reactor::run(&mut allocator, &socket_file),
            fs::remove_file(&socket_file)
                .map_io_err(|| format!("Failed to delete server socket: {socket_file:?}"))
                .map_err(CliError::from),
            allocator.shutdown(),
            server_guard.shutdown(),
        ]
        .into_iter()
        .flat_map(Result::err)
        .rev()
        .collect::<Vec<_>>(),
    )
}
