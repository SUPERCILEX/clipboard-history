use std::{io, ptr, ptr::NonNull};

use io_uring::Submitter;
use rustix::{
    io_uring::io_uring_buf,
    mm::{MapFlags, ProtFlags, mmap_anonymous, munmap},
};

use crate::io_uring::buf_ring::BufRing;

pub fn register_buf_ring(
    submitter: &Submitter,
    ring_entries: u16,
    bgid: u16,
    entry_size: u32,
) -> io::Result<BufRing> {
    let bytes = || {
        usize::from(ring_entries).checked_mul(
            size_of::<io_uring_buf>().checked_add(usize::try_from(entry_size).unwrap())?,
        )
    };

    let ring = MmapAnon::new(bytes().ok_or(io::ErrorKind::InvalidInput)?)?;
    unsafe {
        submitter.register_buf_ring(ring.ptr.as_ptr() as u64, ring_entries, bgid)?;
    }
    Ok(BufRing::init(ring, ring_entries, entry_size, bgid))
}

#[derive(Debug)]
pub struct MmapAnon {
    ptr: NonNull<u8>,
    len: usize,
}

impl MmapAnon {
    pub fn new(len: usize) -> io::Result<Self> {
        Ok(Self {
            ptr: unsafe {
                NonNull::new_unchecked(mmap_anonymous(
                    ptr::null_mut(),
                    len,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::PRIVATE,
                )?)
            }
            .cast(),
            len,
        })
    }
}

impl Drop for MmapAnon {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

pub mod types {
    use std::slice;

    use rustix::io_uring::{RecvmsgOutFlags, io_uring_recvmsg_out};

    /// Helper structure for parsing the result of a multishot
    /// [`opcode::RecvMsg`](crate::opcode::RecvMsg).
    #[derive(Debug)]
    pub struct RecvMsgOutMut<'buf> {
        header: io_uring_recvmsg_out,
        /// The fixed length of the name field, in bytes.
        ///
        /// If the incoming name data is larger than this, it gets truncated to
        /// this. If it is smaller, it gets 0-padded to fill the whole
        /// field. In either case, this fixed amount of space is
        /// reserved in the result buffer.
        msghdr_name_len: usize,

        /// Message control data, with the same semantics as
        /// `msghdr.msg_control`.
        pub control_data: &'buf mut [u8],
        /// Message payload, as buffered by the kernel.
        pub payload_data: &'buf mut [u8],
    }

    impl<'buf> RecvMsgOutMut<'buf> {
        const DATA_START: usize = size_of::<io_uring_recvmsg_out>();

        /// Parse the data buffered upon completion of a `RecvMsg` multishot
        /// operation.
        ///
        /// `buffer` is the whole buffer previously provided to the ring, while
        /// `msghdr` is the same content provided as input to the
        /// corresponding SQE (only `msg_namelen` and `msg_controllen`
        /// fields are relevant).
        #[allow(clippy::result_unit_err)]
        #[allow(clippy::useless_conversion)]
        pub fn parse(buffer: &'buf mut [u8], msghdr: &libc::msghdr) -> Result<Self, ()> {
            let msghdr_name_len = usize::try_from(msghdr.msg_namelen).unwrap();
            let msghdr_control_len = usize::try_from(msghdr.msg_controllen).unwrap();

            if Self::DATA_START
                .checked_add(msghdr_name_len)
                .and_then(|acc| acc.checked_add(msghdr_control_len))
                .is_none_or(|header_len| buffer.len() < header_len)
            {
                return Err(());
            }
            // SAFETY: buffer (minimum) length is checked here above.
            let header = unsafe {
                buffer
                    .as_ptr()
                    .cast::<io_uring_recvmsg_out>()
                    .read_unaligned()
            };

            // min is used because the header may indicate the true size of the data
            // while what we received was truncated.
            let (_name_data, control_start) = {
                let name_start = Self::DATA_START;
                let name_data_len =
                    usize::min(usize::try_from(header.namelen).unwrap(), msghdr_name_len);
                let name_field_end = name_start + msghdr_name_len;
                (
                    unsafe {
                        slice::from_raw_parts_mut(
                            buffer.as_mut_ptr().add(name_start),
                            name_data_len,
                        )
                    },
                    name_field_end,
                )
            };
            let (control_data, payload_start) = {
                let control_data_len = usize::min(
                    usize::try_from(header.controllen).unwrap(),
                    msghdr_control_len,
                );
                let control_field_end = control_start + msghdr_control_len;
                (
                    unsafe {
                        slice::from_raw_parts_mut(
                            buffer.as_mut_ptr().add(control_start),
                            control_data_len,
                        )
                    },
                    control_field_end,
                )
            };
            let payload_data = {
                let payload_data_len = usize::min(
                    usize::try_from(header.payloadlen).unwrap(),
                    buffer.len() - payload_start,
                );
                unsafe {
                    slice::from_raw_parts_mut(
                        buffer.as_mut_ptr().add(payload_start),
                        payload_data_len,
                    )
                }
            };

            Ok(Self {
                header,
                msghdr_name_len,
                control_data,
                payload_data,
            })
        }

        /// Return whether the incoming name data was larger than the provided
        /// limit/buffer.
        ///
        /// When `true`, data returned by `name_data()` is truncated and
        /// incomplete.
        pub const fn is_name_data_truncated(&self) -> bool {
            self.header.namelen as usize > self.msghdr_name_len
        }

        /// Return whether the incoming control data was larger than the
        /// provided limit/buffer.
        ///
        /// When `true`, data returned by `control_data()` is truncated and
        /// incomplete.
        pub const fn is_control_data_truncated(&self) -> bool {
            self.header.flags.contains(RecvmsgOutFlags::CTRUNC)
        }

        /// Return whether the incoming payload was larger than the provided
        /// limit/buffer.
        ///
        /// When `true`, data returned by `payload_data()` is truncated and
        /// incomplete.
        pub const fn is_payload_truncated(&self) -> bool {
            self.header.flags.contains(RecvmsgOutFlags::TRUNC)
        }
    }
}

pub mod buf_ring {
    use std::{
        convert::TryFrom,
        io,
        marker::PhantomData,
        mem::ManuallyDrop,
        num::Wrapping,
        ops::{Deref, DerefMut},
        slice,
        sync::atomic,
    };

    use io_uring::Submitter;
    use rustix::io_uring::{IORING_CQE_BUFFER_SHIFT, io_uring_buf};

    use crate::io_uring::MmapAnon;

    pub struct BufRing {
        ring: MmapAnon,
        ring_entries: u16,
        entry_size: u32,
        group_id: u16,
    }

    impl BufRing {
        pub(super) fn init(
            ring: MmapAnon,
            ring_entries: u16,
            entry_size: u32,
            group_id: u16,
        ) -> Self {
            let mut this = Self {
                ring,
                ring_entries,
                entry_size,
                group_id,
            };

            {
                let mut s = this.submissions();
                for i in 0u16..ring_entries {
                    let buf = unsafe { s.recycle_by_index_(i) };
                    buf.len = entry_size;
                }
            }

            this
        }

        #[allow(clippy::cast_ptr_alignment)]
        pub fn submissions(&mut self) -> BufRingSubmissions<'_> {
            let ring_ptr = self.ring.ptr.as_ptr().cast::<io_uring_buf>();
            let tail_ptr = unsafe { self.ring.ptr.as_ptr().add(8 + 4 + 2) };
            let ring_entries = usize::from(self.ring_entries);
            BufRingSubmissions {
                ring_ptr,
                buf_ptr: unsafe { ring_ptr.add(ring_entries).cast() },
                tail_ptr: tail_ptr.cast::<atomic::AtomicU16>(),

                tail: Wrapping(usize::from(unsafe { *tail_ptr.cast::<u16>() })),
                tail_mask: ring_entries - 1,
                entry_size: usize::try_from(self.entry_size).unwrap(),

                _marker: PhantomData,
            }
        }

        pub fn unregister(self, submitter: &Submitter) -> io::Result<()> {
            submitter.unregister_buf_ring(self.group_id)
        }
    }

    pub struct BufRingSubmissions<'ctx> {
        ring_ptr: *mut io_uring_buf,
        buf_ptr: *mut libc::c_void,
        tail_ptr: *const atomic::AtomicU16,

        tail: Wrapping<usize>,
        tail_mask: usize,
        entry_size: usize,

        _marker: PhantomData<&'ctx ()>,
    }

    impl<'a> BufRingSubmissions<'a> {
        pub fn sync(&mut self) {
            #[allow(clippy::cast_possible_truncation)]
            unsafe { &*self.tail_ptr }.store(self.tail.0 as u16, atomic::Ordering::Release);
        }

        pub unsafe fn get(&mut self, flags: u32, len: usize) -> Buf<'_, 'a> {
            let index = Self::flags_to_index(flags);
            let buf = unsafe { self.buf_ptr.add(usize::from(index) * self.entry_size) };
            Buf {
                data: unsafe { slice::from_raw_parts_mut(buf.cast(), len) },
                index,
                submissions: self,
            }
        }

        pub unsafe fn recycle_by_index(&mut self, index: u16) {
            unsafe {
                self.recycle_by_index_(index);
            }
        }

        unsafe fn recycle_by_index_(&mut self, index: u16) -> &mut io_uring_buf {
            let uindex = usize::from(index);
            {
                let next_buf = unsafe { &mut *self.ring_ptr.add(self.tail.0 & self.tail_mask) };
                next_buf.addr = unsafe { self.buf_ptr.add(uindex * self.entry_size) } as u64;
                next_buf.bid = index;
            }
            self.tail += &1;

            unsafe { &mut *self.ring_ptr.add(uindex) }
        }

        fn flags_to_index(flags: u32) -> u16 {
            u16::try_from(flags >> IORING_CQE_BUFFER_SHIFT).unwrap()
        }
    }

    impl Drop for BufRingSubmissions<'_> {
        fn drop(&mut self) {
            self.sync();
        }
    }

    pub struct Buf<'a, 'b> {
        data: &'a mut [u8],
        index: u16,
        submissions: &'a mut BufRingSubmissions<'b>,
    }

    impl Deref for Buf<'_, '_> {
        type Target = [u8];

        fn deref(&self) -> &Self::Target {
            self.data
        }
    }

    impl DerefMut for Buf<'_, '_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.data
        }
    }

    impl Buf<'_, '_> {
        pub fn into_index(self) -> u16 {
            let me = ManuallyDrop::new(self);
            me.index
        }
    }

    impl Drop for Buf<'_, '_> {
        fn drop(&mut self) {
            unsafe {
                self.submissions.recycle_by_index(self.index);
            }
        }
    }
}
