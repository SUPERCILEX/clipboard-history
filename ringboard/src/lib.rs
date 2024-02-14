#![feature(core_io_borrowed_buf, read_buf)]
#![feature(write_all_vectored)]
#![allow(clippy::missing_errors_doc)]

use std::{borrow::Cow, io, num::ParseIntError, path::PathBuf};

use thiserror::Error;
pub use utils::read_server_pid;

pub mod dirs;
pub mod protocol;
pub mod ring;
mod utils;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("An I/O error occurred")]
    Io {
        error: io::Error,
        context: Cow<'static, str>,
    },
    #[error("The provided file was not a Ringboard database: {file:?}")]
    NotARingboard { file: PathBuf },
    #[error("Invalid PID")]
    InvalidPidError {
        error: ParseIntError,
        context: Cow<'static, str>,
    },
}

pub trait IoErr<Out> {
    fn map_io_err<I: Into<Cow<'static, str>>>(self, f: impl FnOnce() -> I) -> Out;
}

impl<T> IoErr<Result<T>> for std::result::Result<T, io::Error> {
    fn map_io_err<I: Into<Cow<'static, str>>>(self, context: impl FnOnce() -> I) -> Result<T> {
        self.map_err(|error| Error::Io {
            error,
            context: context().into(),
        })
    }
}

impl<T> IoErr<Result<T>> for rustix::io::Result<T> {
    fn map_io_err<I: Into<Cow<'static, str>>>(self, context: impl FnOnce() -> I) -> Result<T> {
        self.map_err(io::Error::from).map_io_err(context)
    }
}
