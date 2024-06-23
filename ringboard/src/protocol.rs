use arrayvec::ArrayString;

use crate::AsBytes;

pub const VERSION: u8 = 0;

#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum RingKind {
    Favorites,
    Main,
}

impl RingKind {
    #[must_use]
    pub const fn file_name(&self) -> &'static str {
        match self {
            Self::Main => "main.ring",
            Self::Favorites => "favorites.ring",
        }
    }

    #[must_use]
    pub const fn default_max_entries(&self) -> u32 {
        match self {
            Self::Main => 262_142,
            Self::Favorites => 1022,
        }
    }
}

// https://github.com/patrickmccallum/mimetype-io/blob/3a8176e6dd5d183b62a6d78013504128d96e9889/src/mimeData.json
// The longest mime type found was 73 bytes long, so this should be more than
// enough while still letting the Request fit in two cache lines.
pub type MimeType = ArrayString<120>;

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum Request {
    Add { to: RingKind, mime_type: MimeType },
    MoveToFront { id: u64, to: Option<RingKind> },
    Swap { id1: u64, id2: u64 },
    Remove { id: u64 },
    ReloadSettings,
    GarbageCollect,
}

const _: () = assert!(size_of::<Request>() == 128);

#[repr(C)]
#[derive(Copy, Clone, Debug)]
#[must_use]
pub enum AddResponse {
    Success { id: u64 },
}

#[repr(u8)]
#[derive(Copy, Clone, thiserror::Error, Debug)]
pub enum IdNotFoundError {
    #[error("Invalid ring ID: {0}.")]
    Ring(u32),
    #[error("Invalid entry ID: {0}.")]
    Entry(u32),
}

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
#[must_use]
pub enum MoveToFrontResponse {
    Success { id: u64 },
    Error(IdNotFoundError),
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
#[must_use]
pub struct SwapResponse {
    pub error1: Option<IdNotFoundError>,
    pub error2: Option<IdNotFoundError>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
#[must_use]
pub struct RemoveResponse {
    pub error: Option<IdNotFoundError>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
#[must_use]
pub struct ReloadSettingsResponse {
    // TODO add invalid config errors
    pub error: Option<()>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
#[must_use]
pub struct GarbageCollectResponse {
    pub bytes_freed: u64,
}

#[must_use]
pub fn composite_id(kind: RingKind, id: u32) -> u64 {
    ((kind as u64) << 32) | u64::from(id)
}

pub fn decompose_id(id: u64) -> Result<(RingKind, u32), IdNotFoundError> {
    match id >> 32 {
        0 => Ok(RingKind::Favorites),
        1 => Ok(RingKind::Main),
        ring => Err(IdNotFoundError::Ring(u32::try_from(ring).unwrap())),
    }
    .map(|ring| (ring, u32::try_from(id & u64::from(u32::MAX)).unwrap()))
}

impl AsBytes for Request {}

impl AsBytes for AddResponse {}
impl AsBytes for MoveToFrontResponse {}
impl AsBytes for SwapResponse {}
impl AsBytes for RemoveResponse {}
impl AsBytes for ReloadSettingsResponse {}
