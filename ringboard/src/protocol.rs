pub const VERSION: u8 = 0;

#[repr(u8)]
#[derive(Debug)]
pub enum Request {
    Add,
}
