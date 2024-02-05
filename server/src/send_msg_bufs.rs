use std::{mem, mem::MaybeUninit, ptr};

use arrayvec::ArrayVec;

pub struct SendMsgBufs {
    allocated_mask: u64,
    bufs: [MaybeUninit<Vec<u8>>; 64],
    pool: ArrayVec<Vec<u8>, 64>,
}

pub type Token = u8;

impl SendMsgBufs {
    const TOKEN_MASK: u64 = 64 - 1;

    pub const fn new() -> Self {
        const INIT: MaybeUninit<Vec<u8>> = MaybeUninit::uninit();
        Self {
            allocated_mask: 0,
            bufs: [INIT; 64],
            pool: ArrayVec::new_const(),
        }
    }

    pub fn alloc<Control: FnOnce(&mut [MaybeUninit<u8>]), Data: FnOnce(&mut [MaybeUninit<u8>])>(
        &mut self,
        control_bytes: usize,
        data_bytes: usize,
        control: Control,
        data: Data,
    ) -> Result<(Token, *const libc::msghdr), ()> {
        let token = u8::try_from(self.allocated_mask.trailing_ones()).unwrap();
        if u32::from(token) == u64::BITS {
            return Err(());
        }

        let metadata_end = mem::size_of::<libc::msghdr>() + mem::size_of::<libc::iovec>();
        let control_end = metadata_end + control_bytes;
        let mut buf = self.pool.pop().unwrap_or_default();
        buf.reserve_exact(control_end + data_bytes);

        control(&mut buf.spare_capacity_mut()[metadata_end..]);
        data(&mut buf.spare_capacity_mut()[control_end..]);

        {
            let ptr = buf.spare_capacity_mut().as_mut_ptr();

            let hdr = libc::msghdr {
                msg_name: ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: unsafe { ptr.add(mem::size_of::<libc::msghdr>()).cast() },
                msg_iovlen: 1,
                msg_control: unsafe { ptr.add(metadata_end).cast() },
                msg_controllen: control_bytes,
                msg_flags: 0,
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    ptr::from_ref(&hdr).cast(),
                    ptr,
                    mem::size_of::<libc::msghdr>(),
                )
            }
            let iov = libc::iovec {
                iov_base: unsafe { ptr.add(control_end).cast() },
                iov_len: data_bytes,
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    ptr::from_ref(&iov).cast(),
                    ptr,
                    mem::size_of::<libc::iovec>(),
                )
            }
        }

        let ptr = buf.as_ptr();
        self.allocated_mask |= 1 << token;
        self.bufs[usize::from(token)].write(buf);
        Ok((token, ptr.cast()))
    }

    pub unsafe fn free(&mut self, token: u64) {
        let token = u8::try_from(token & Self::TOKEN_MASK).unwrap();
        self.allocated_mask &= !(1 << token);
        self.pool
            .push(unsafe { self.bufs[usize::from(token)].assume_init_read() });
    }
}
