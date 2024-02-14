use std::{
    ffi::CStr,
    io::Write,
    os::fd::{AsFd, AsRawFd, RawFd},
};

use rustix::{
    fs::{linkat, AtFlags, CWD},
    path::Arg,
};

pub fn link_tmp_file<Fd: AsFd, P: Arg>(tmp_file: Fd, path: P) -> rustix::io::Result<()> {
    const _: () = assert!(RawFd::BITS <= i32::BITS);
    let mut buf = [0u8; "/proc/self/fd/".len() + "-2147483648".len() + 1];
    write!(
        buf.as_mut_slice(),
        "/proc/self/fd/{}",
        tmp_file.as_fd().as_raw_fd()
    )
    .unwrap();

    linkat(
        CWD,
        unsafe { CStr::from_ptr(buf.as_ptr().cast()) },
        CWD,
        path,
        AtFlags::SYMLINK_FOLLOW,
    )
}
