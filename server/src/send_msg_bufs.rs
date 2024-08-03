use std::{
    mem,
    mem::{ManuallyDrop, MaybeUninit},
    ptr,
};

use bitvec::{array::BitArray, BitArr};
use log::trace;
use smallvec::SmallVec;

use crate::reactor::{MAX_NUM_BUFS_PER_CLIENT, MAX_NUM_CLIENTS};

const CAP: usize = MAX_NUM_CLIENTS as usize * MAX_NUM_BUFS_PER_CLIENT as usize;

pub struct SendMsgBufs {
    allocated_mask: BitArr!(for CAP),
    bufs: [MaybeUninit<LengthlessVec>; CAP],
    pool: SmallVec<LengthlessVec, 4>,
}

pub type Token = u8;
const _: () = assert!(CAP <= (1 << Token::BITS));
pub type SendBufAllocation = (Token, *const libc::msghdr);

impl SendMsgBufs {
    const TOKEN_MASK: u64 = CAP as u64 - 1;

    pub const fn new() -> Self {
        Self {
            allocated_mask: BitArray::ZERO,
            bufs: [const { MaybeUninit::uninit() }; CAP],
            pool: SmallVec::new(),
        }
    }

    pub fn alloc<Control: FnOnce(&mut Vec<u8>), Data: FnOnce(&mut Vec<u8>)>(
        &mut self,
        control: Control,
        data: Data,
    ) -> Result<SendBufAllocation, ()> {
        let token = self.allocated_mask.leading_ones();
        trace!("Allocating send buffer {token}.");
        if token == CAP {
            return Err(());
        }
        let mut buf = self
            .pool
            .pop()
            .map(LengthlessVec::into_vec)
            .unwrap_or_default();

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

        debug_assert!(!self.allocated_mask[token]);
        self.allocated_mask.set(token, true);
        self.bufs[token].write(buf.into());
        Ok((u8::try_from(token).unwrap(), ptr.cast()))
    }

    pub unsafe fn free(&mut self, token: u64) {
        let token = usize::try_from(token & Self::TOKEN_MASK).unwrap();
        trace!("Freeing send buffer {token}.");

        debug_assert!(self.allocated_mask[token]);
        self.allocated_mask.set(token, false);

        let v = unsafe { self.bufs[token].assume_init_read() };
        self.pool.push(v);
    }

    pub fn trim(&mut self) {
        self.pool = SmallVec::new();
    }
}

impl Drop for SendMsgBufs {
    fn drop(&mut self) {
        for i in self.allocated_mask.iter_ones() {
            unsafe {
                self.bufs[i].assume_init_drop();
            }
        }
    }
}

struct LengthlessVec {
    ptr: *mut u8,
    cap: usize,
}

impl LengthlessVec {
    fn into_vec(self) -> Vec<u8> {
        let me = ManuallyDrop::new(self);
        unsafe { me.as_vec_() }
    }

    unsafe fn as_vec_(&self) -> Vec<u8> {
        let Self { ptr, cap } = *self;
        unsafe { Vec::from_raw_parts(ptr, 0, cap) }
    }
}

impl From<Vec<u8>> for LengthlessVec {
    fn from(value: Vec<u8>) -> Self {
        let (ptr, _len, cap) = value.into_raw_parts();
        Self { ptr, cap }
    }
}

impl Drop for LengthlessVec {
    fn drop(&mut self) {
        unsafe { self.as_vec_() };
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
            }
            bufs.trim();
        }
    }
}
