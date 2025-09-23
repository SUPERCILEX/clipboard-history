use std::{borrow::Cow, io, num::ParseIntError};

use thiserror::Error;
pub use utils::*;
pub use views::{BucketAndIndex, PathView, RingAndIndex, StringView};

use crate::protocol::IdNotFoundError;

pub mod dirs;
pub mod polyfills;
pub mod protocol;
pub mod ring;
mod utils;
mod views;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("an I/O error occurred")]
    Io {
        error: io::Error,
        context: Cow<'static, str>,
    },
    #[error("invalid PID")]
    InvalidPidError {
        error: ParseIntError,
        context: Cow<'static, str>,
    },
    #[error("ID not found")]
    IdNotFound(#[from] IdNotFoundError),
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

#[cfg(feature = "error-stack")]
mod error_stack_compat {
    use error_stack::Report;

    use crate::{Error, protocol::IdNotFoundError};

    impl Error {
        pub fn into_report<W: core::error::Error + Send + Sync + 'static>(
            self,
            wrapper: W,
        ) -> Report<W> {
            match self {
                Self::Io { error, context } => {
                    Report::new(error).attach(context).change_context(wrapper)
                }
                Self::InvalidPidError { error, context } => {
                    Report::new(error).attach(context).change_context(wrapper)
                }
                Self::IdNotFound(IdNotFoundError::Ring(id)) => {
                    Report::new(wrapper).attach(format!("Unknown ring: {id}"))
                }
                Self::IdNotFound(IdNotFoundError::Entry(id)) => {
                    Report::new(wrapper).attach(format!("Unknown entry: {id}"))
                }
            }
        }
    }
}
