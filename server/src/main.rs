#![feature(write_all_vectored)]
#![feature(vec_into_raw_parts)]
#![feature(let_chains)]

use std::{borrow::Cow, collections::VecDeque, fs, path::PathBuf};

use error_stack::Report;
use log::info;
use ringboard_core::{Error, IoErr, dirs::data_dir};
use rustix::process::{Pid, chdir};
use thiserror::Error;

use crate::{allocator::Allocator, startup::claim_server_ownership};

mod allocator;
mod io_uring;
mod reactor;
mod requests;
mod send_msg_bufs;
mod startup;

#[cfg(feature = "trace")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

#[derive(Error, Debug)]
enum CliError {
    #[error("{0}")]
    Core(#[from] Error),
    #[error("server already running at {pid:?}")]
    ServerAlreadyRunning { pid: Pid, lock_file: PathBuf },
    #[error("multiple errors occurred")]
    Multiple(Vec<CliError>),
    #[error("internal error")]
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
        CliError::Core(e) => e.into_report(wrapper),
        CliError::ServerAlreadyRunning { pid: _, lock_file } => Report::new(wrapper)
            .attach_printable(
                "Unable to safely start server: please shut down the existing instance. If \
                 something has gone terribly wrong, please create an empty server lock file to \
                 initiate the recovery sequence on the next startup.",
            )
            .attach_printable(format!("Lock file: {lock_file:?}")),
        CliError::Multiple(errs) => {
            let mut errs = VecDeque::from(errs);
            let mut report = into_report(errs.pop_front().unwrap_or_else(|| CliError::Internal {
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
    info!("Starting Ringboard server v{}.", env!("CARGO_PKG_VERSION"));

    {
        let data_dir = data_dir();
        info!("Using database in {data_dir:?}.");

        fs::create_dir_all(&data_dir)
            .map_io_err(|| format!("Failed to create data directory: {data_dir:?}"))?;
        chdir(&data_dir)
            .map_io_err(|| format!("Failed to change working directory: {data_dir:?}"))?;
    }
    let server_guard = claim_server_ownership()?;
    info!("Acquired server lock.");

    let mut allocator = Allocator::open()?;
    into_result(
        [
            reactor::run(&mut allocator),
            allocator.shutdown(),
            server_guard.shutdown(),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>(),
    )
}
