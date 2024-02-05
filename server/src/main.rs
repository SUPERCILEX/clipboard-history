use std::{borrow::Cow, fs, num::NonZeroU32, path::PathBuf};

use clipboard_history_core::{
    dirs::{data_dir, socket_file},
    Error, IoErr,
};
use error_stack::Report;
use log::info;
use thiserror::Error;

use crate::{path_view::PathView, startup::claim_server_ownership};

mod path_view;
mod reactor;
mod requests;
mod send_msg_bufs;
mod startup;

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] Error),
    #[error("The server is already running (PID {pid})")]
    ServerAlreadyRunning { pid: NonZeroU32, lock_file: PathBuf },
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

    run().map_err(|e| {
        let wrapper = Wrapper::W(e.to_string());
        match e {
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
                     something has gone terribly wrong, please create an empty server lock file \
                     to initiate the recovery sequence on the next startup.",
                )
                .attach_printable(format!("Lock file: {lock_file:?}")),
            CliError::Internal { context } => Report::new(wrapper)
                .attach_printable(context)
                .attach_printable("Please report this bug at https://github.com/SUPERCILEX/clipboard-history/issues/new"),
        }
    })
}

fn run() -> Result<(), CliError> {
    let mut data_dir = data_dir();
    fs::create_dir_all(&data_dir)
        .map_io_err(|| format!("Failed to create data directory: {data_dir:?}"))?;
    let server_guard = claim_server_ownership(&PathView::new(&mut data_dir, "server.lock"))?
        .unwrap_or_else(|| todo!());
    let socket_file = socket_file();
    info!("Acquired server lock.");

    let result = reactor::run(data_dir, &socket_file);
    let _ = fs::remove_file(socket_file);
    server_guard.shutdown()?;
    result
}
