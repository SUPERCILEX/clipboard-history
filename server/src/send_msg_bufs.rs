use std::{mem, mem::ManuallyDrop, ptr, ptr::NonNull};

use arrayvec::ArrayVec;
use log::trace;
use smallvec::SmallVec;

use crate::reactor::{MAX_NUM_BUFS_PER_CLIENT, MAX_NUM_CLIENTS};

pub struct SendMsgBufs {
    bufs: [[Option<LengthlessVec>; MAX_NUM_BUFS_PER_CLIENT as usize]; MAX_NUM_CLIENTS as usize],
    alloc_counts: [u8; MAX_NUM_CLIENTS as usize],
    pending_bufs: [ArrayVec<SendBufAllocation, { MAX_NUM_BUFS_PER_CLIENT as usize }>;
        MAX_NUM_CLIENTS as usize],
    pool: SmallVec<LengthlessVec, 4>,
}

pub type PendingBufAllocation = (Vec<u8>, *const libc::msghdr);
pub type SendBufAllocation = (u8, *const libc::msghdr);

impl SendMsgBufs {
    const TOKEN_MASK: u8 = MAX_NUM_BUFS_PER_CLIENT - 1;

    pub const fn new() -> Self {
        Self {
            bufs: [const { [const { None }; MAX_NUM_BUFS_PER_CLIENT as usize] };
                MAX_NUM_CLIENTS as usize],
            alloc_counts: [0; MAX_NUM_CLIENTS as usize],
            pending_bufs: [const { ArrayVec::new_const() }; MAX_NUM_CLIENTS as usize],
            pool: SmallVec::new(),
        }
    }

    pub fn drain_pending_sends(
        &mut self,
        client: u8,
        max: usize,
    ) -> impl ExactSizeIterator<Item = SendBufAllocation> + '_ {
        let pending = &mut self.pending_bufs[usize::from(client)];
        pending.drain(..max.min(pending.len()))
    }

    pub fn has_pending_sends(&self, client: u8) -> bool {
        !self.pending_bufs[usize::from(client)].is_empty()
    }

    pub fn has_ready_block(&self, client: u8) -> bool {
        let client = usize::from(client);
        self.pending_bufs[client].len() == self.alloc_counts[client].into()
    }

    pub fn has_outstanding_sends(&self, client: u8) -> bool {
        self.alloc_counts[usize::from(client)] > 0
    }

    pub fn init_buf<Control: FnOnce(&mut Vec<u8>), Data: FnOnce(&mut Vec<u8>)>(
        &mut self,
        control: Control,
        data: Data,
    ) -> PendingBufAllocation {
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

        (buf, ptr.cast())
    }

    pub fn alloc(&mut self, client: u8, token: u64, (buf, ptr): PendingBufAllocation) {
        let client = usize::from(client);
        let token = usize::try_from(token & u64::from(Self::TOKEN_MASK)).unwrap();
        trace!("Allocating send buffer {token} for client {client}.");

        debug_assert!(self.bufs[client][token].is_none());
        self.bufs[client][token] = Some(buf.into());
        self.alloc_counts[client] += 1;
        self.pending_bufs[client].push((u8::try_from(token).unwrap(), ptr));
    }

    pub unsafe fn free(&mut self, client: u8, token: u64) {
        let client = usize::from(client);
        let token = usize::try_from(token & u64::from(Self::TOKEN_MASK)).unwrap();
        trace!("Freeing send buffer {token} for client {client}.");

        self.alloc_counts[client] -= 1;
        let v = self.bufs[client][token].take().unwrap();
        self.pool.push(v);
    }

    pub fn trim(&mut self) {
        self.pool = SmallVec::new();
    }
}

struct LengthlessVec {
    ptr: NonNull<u8>,
    cap: usize,
}

impl LengthlessVec {
    fn into_vec(self) -> Vec<u8> {
        let me = ManuallyDrop::new(self);
        unsafe { me.as_vec_() }
    }

    unsafe fn as_vec_(&self) -> Vec<u8> {
        let Self { ptr, cap } = *self;
        unsafe { Vec::from_raw_parts(ptr.as_ptr(), 0, cap) }
    }
}

#[allow(clippy::fallible_impl_from)]
impl From<Vec<u8>> for LengthlessVec {
    fn from(value: Vec<u8>) -> Self {
        let (ptr, _len, cap) = value.into_raw_parts();
        Self {
            ptr: NonNull::new(ptr).unwrap(),
            cap,
        }
    }
}

impl Drop for LengthlessVec {
    fn drop(&mut self) {
        unsafe { self.as_vec_() };
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        reactor::{MAX_NUM_BUFS_PER_CLIENT, MAX_NUM_CLIENTS},
        send_msg_bufs::SendMsgBufs,
    };

    #[test]
    fn fill() {
        let mut bufs = SendMsgBufs::new();
        for client in 0..MAX_NUM_CLIENTS {
            for i in 0..MAX_NUM_BUFS_PER_CLIENT {
                let pending = bufs.init_buf(
                    |control| control.extend(1..=69),
                    |data| data.extend((0..420).map(|_| 0xDE)),
                );
                bufs.alloc(client, i.into(), pending);
            }
        }
    }

    #[test]
    fn free_random() {
        let mut bufs = SendMsgBufs::new();

        for i in 0..3 {
            let pending = bufs.init_buf(
                |control| control.extend(1..=69),
                |data| data.extend((0..420).map(|_| 0xDE)),
            );
            bufs.alloc(0, i, pending);
        }

        let token = bufs.drain_pending_sends(0, usize::MAX).nth(1).unwrap();
        unsafe {
            bufs.free(0, token.0.into());
        }
    }

    #[test]
    fn stress() {
        let mut bufs = SendMsgBufs::new();
        for control_len in 0..50 {
            for data_len in 0..50 {
                let control_data = 0..control_len;
                let data = 0..data_len;
                let pending = bufs.init_buf(
                    |buf| buf.extend(control_data.clone()),
                    |buf| buf.extend(data.clone()),
                );
                bufs.alloc(0, data_len.into(), pending);
                let (token, hdr) = bufs.drain_pending_sends(0, usize::MAX).next().unwrap();

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

                unsafe { bufs.free(0, token.into()) };
            }
            bufs.trim();
        }
    }
}
