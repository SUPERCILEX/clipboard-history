use std::borrow::Cow;

pub use ring_reader::{
    DatabaseReader, Entry, EntryReader, Kind, LoadedEntry, RingReader, is_text_mime,
};
pub use ringboard_core as core;
use ringboard_core::protocol::IdNotFoundError;
#[cfg(feature = "search")]
pub use search::search;
use thiserror::Error;

pub mod api;
#[cfg(feature = "config")]
pub mod config;
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
    #[error("protocol version mismatch")]
    VersionMismatch { expected: u8, actual: u8 },
    #[error("invalid server response")]
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
                Self::VersionMismatch { expected, actual } => Report::new(wrapper)
                    .attach_printable(format!("Expected v{expected} but got v{actual}.")),
            }
        }
    }
}
