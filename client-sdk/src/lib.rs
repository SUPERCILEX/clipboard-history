use std::borrow::Cow;

pub use ring_reader::{DatabaseReader, Entry, EntryReader, Kind, LoadedEntry, RingReader};
pub use ringboard_core as core;
use ringboard_core::{protocol, protocol::IdNotFoundError};
#[cfg(feature = "search")]
pub use search::search;
use thiserror::Error;

pub mod api;
#[cfg(feature = "deduplication")]
pub mod duplicate_detection;
mod ring_reader;
#[cfg(feature = "search")]
pub mod search;
#[cfg(feature = "ui")]
pub mod ui_actor;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("{0}")]
    Core(#[from] ringboard_core::Error),
    #[error(
        "Protocol version mismatch: expected {} but got {actual}.",
        protocol::VERSION
    )]
    VersionMismatch { actual: u8 },
    #[error("The server returned an invalid response.")]
    InvalidResponse { context: Cow<'static, str> },
}

impl From<IdNotFoundError> for ClientError {
    fn from(value: IdNotFoundError) -> Self {
        Self::Core(ringboard_core::Error::IdNotFound(value))
    }
}

#[cfg(feature = "error-stack")]
mod error_stack_compat {
    use error_stack::{Context, Report};

    use crate::ClientError;

    impl ClientError {
        pub fn into_report<W: Context>(self, wrapper: W) -> Report<W> {
            match self {
                Self::Core(e) => e.into_report(wrapper),
                Self::InvalidResponse { context } => Report::new(wrapper).attach_printable(context),
                Self::VersionMismatch { actual: _ } => Report::new(wrapper),
            }
        }
    }
}
