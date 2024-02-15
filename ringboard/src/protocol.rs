use std::mem;

use arrayvec::ArrayString;

pub const VERSION: u8 = 0;

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum RingKind {
    Favorites,
    Main,
}

pub type MimeType = ArrayString<120>;

#[repr(u8)]
#[derive(Debug)]
pub enum Request {
    Add { kind: RingKind, mime_type: MimeType },
}

const _: () = assert!(mem::size_of::<Request>() == 128);

pub fn composite_id(kind: RingKind, id: u32) -> u64 {
    ((kind as u64) << 32) | u64::from(id)
}
