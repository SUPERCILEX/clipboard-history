use std::{mem, mem::MaybeUninit, ptr};

use arrayvec::ArrayVec;
use log::trace;

const CAP: usize = u64::BITS as usize;

pub struct SendMsgBufs {
    allocated_mask: u64,
    bufs: [MaybeUninit<Vec<u8>>; CAP],
    pool: ArrayVec<Vec<u8>, CAP>,
}

pub type Token = u8;
pub type SendBufAllocation = (Token, *const libc::msghdr);

impl SendMsgBufs {
    const TOKEN_MASK: u64 = CAP as u64 - 1;

    pub const fn new() -> Self {
        const INIT: MaybeUninit<Vec<u8>> = MaybeUninit::uninit();
        Self {
            allocated_mask: 0,
            bufs: [INIT; CAP],
            pool: ArrayVec::new_const(),
        }
    }

    pub fn alloc<Control: FnOnce(&mut Vec<u8>), Data: FnOnce(&mut Vec<u8>)>(
        &mut self,
        control: Control,
        data: Data,
    ) -> Result<SendBufAllocation, ()> {
        let token = u8::try_from(self.allocated_mask.trailing_ones()).unwrap();
        trace!("Allocating send buffer {token}.");
        if usize::from(token) == CAP {
            return Err(());
        }
        let mut buf = self.pool.pop().unwrap_or_default();

        control(&mut buf);
        let control_len = buf.len();
        data(&mut buf);
        let data_len = buf.len() - control_len;

        let ptr = {
            let metadata_size = size_of::<libc::msghdr>() + size_of::<libc::iovec>();
            let align_offset = loop {
                let old_ptr = buf.as_ptr();
                let align_offset = buf
                    .spare_capacity_mut()
                    .as_ptr()
                    .align_offset(align_of::<libc::msghdr>());
                buf.reserve(align_offset + metadata_size);

                if old_ptr == buf.as_ptr() {
                    break align_offset;
                }
            };

            let ptr = unsafe { buf.spare_capacity_mut().as_mut_ptr().add(align_offset) };
            #[allow(clippy::useless_conversion)]
            let hdr = {
                let mut hdr = unsafe { mem::zeroed::<libc::msghdr>() };
                hdr.msg_name = ptr::null_mut();
                hdr.msg_namelen = 0;
                hdr.msg_iov = unsafe { ptr.add(size_of::<libc::msghdr>()).cast() };
                hdr.msg_iovlen = 1;
                hdr.msg_control = buf.as_mut_ptr().cast();
                hdr.msg_controllen = control_len.try_into().unwrap();
                hdr.msg_flags = 0;
                hdr
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    ptr::from_ref(&hdr).cast(),
                    ptr,
                    size_of::<libc::msghdr>(),
                );
            }

            let iov = libc::iovec {
                iov_base: unsafe { buf.as_mut_ptr().add(control_len).cast() },
                iov_len: data_len,
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    ptr::from_ref(&iov).cast(),
                    ptr.add(size_of::<libc::msghdr>()),
                    size_of::<libc::iovec>(),
                );
            }

            ptr
        };

        self.allocated_mask |= 1 << token;
        self.bufs[usize::from(token)].write(buf);
        Ok((token, ptr.cast()))
    }

    pub unsafe fn free(&mut self, token: u64) {
        let token = u8::try_from(token & Self::TOKEN_MASK).unwrap();
        trace!("Freeing send buffer {token}.");

        self.allocated_mask &= !(1 << token);

        let mut v = unsafe { self.bufs[usize::from(token)].assume_init_read() };
        v.clear();
        self.pool.push(v);
    }

    pub fn trim(&mut self) {
        self.pool.clear();
    }
}

impl Drop for SendMsgBufs {
    fn drop(&mut self) {
        for i in 0..CAP {
            if (self.allocated_mask >> i) & 1 == 1 {
                unsafe {
                    self.bufs[i].assume_init_drop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::send_msg_bufs::{SendMsgBufs, CAP};

    #[test]
    fn fill() {
        let mut bufs = SendMsgBufs::new();
        for _ in 0..CAP {
            bufs.alloc(
                |control| control.extend(1..=69),
                |data| data.extend((0..420).map(|_| 0xDE)),
            )
            .unwrap();
        }

        assert!(
            bufs.alloc(
                |control| control.extend(1..=69),
                |data| data.extend((0..420).map(|_| 0xDE)),
            )
            .is_err()
        );
    }

    #[test]
    fn free_random() {
        let mut bufs = SendMsgBufs::new();

        let tokens = (0..3)
            .map(|_| {
                bufs.alloc(
                    |control| control.extend(1..=69),
                    |data| data.extend((0..420).map(|_| 0xDE)),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();

        unsafe {
            bufs.free(tokens[1].0.into());
        }
    }

    #[test]
    fn stress() {
        let mut bufs = SendMsgBufs::new();
        for control_len in 0..50 {
            for data_len in 0..50 {
                let control_data = 0..control_len;
                let data = 0..data_len;
                let (token, hdr) = bufs
                    .alloc(
                        |buf| buf.extend(control_data.clone()),
                        |buf| buf.extend(data.clone()),
                    )
                    .unwrap();

                let hdr = unsafe { &*hdr };
                assert_eq!(hdr.msg_controllen, usize::from(control_len));
                let iov = unsafe { &*hdr.msg_iov };
                assert_eq!(iov.iov_len, usize::from(data_len));

                for (i, data) in control_data.enumerate() {
                    assert_eq!(unsafe { *hdr.msg_control.add(i).cast::<u8>() }, data);
                }
                for (i, data) in data.enumerate() {
                    assert_eq!(unsafe { *iov.iov_base.add(i).cast::<u8>() }, data);
                }

                unsafe { bufs.free(token.into()) };
                bufs.trim();
            }
        }
    }
}
