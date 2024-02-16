use std::mem;

use arrayvec::ArrayString;

use crate::AsBytes;

pub const VERSION: u8 = 0;

#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum RingKind {
    Favorites,
    Main,
}

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

const _: () = assert!(mem::size_of::<Request>() == 128);

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub enum AddResponse {
    Success { id: u64 },
}

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum IdNotFoundError {
    Ring(u32),
    Entry(u32),
}

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum MoveToFrontResponse {
    Success { id: u64 },
    Error(IdNotFoundError),
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct SwapResponse {
    pub error1: Option<IdNotFoundError>,
    pub error2: Option<IdNotFoundError>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct RemoveResponse {
    pub error: Option<IdNotFoundError>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ReloadSettingsResponse {
    // TODO add invalid config errors
    pub error: Option<()>,
}

#[must_use]
pub fn composite_id(kind: RingKind, id: u32) -> u64 {
    ((kind as u64) << 32) | u64::from(id)
}

impl AsBytes for Request {}

impl AsBytes for AddResponse {}
impl AsBytes for MoveToFrontResponse {}
impl AsBytes for SwapResponse {}
impl AsBytes for RemoveResponse {}
impl AsBytes for ReloadSettingsResponse {}
