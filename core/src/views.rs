use std::{
    fmt::{Debug, Formatter},
    mem::transmute,
};

pub use path::PathView;
pub use string::StringView;

use crate::{
    protocol::{IdNotFoundError, RingKind, composite_id, decompose_id},
    ring::MAX_ENTRIES,
};

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RingAndIndex(u32);

impl RingAndIndex {
    #[must_use]
    pub fn new(ring: RingKind, index: u32) -> Self {
        const {
            assert!(size_of::<RingKind>() == size_of::<u8>());
        }
        debug_assert!(index <= MAX_ENTRIES);

        Self((index << u8::BITS) | (ring as u32))
    }

    pub fn from_id(composite_id: u64) -> Result<Self, IdNotFoundError> {
        decompose_id(composite_id).map(|(ring, id)| Self::new(ring, id))
    }

    #[must_use]
    pub fn ring(self) -> RingKind {
        let ring = u8::try_from(self.0 & u32::from(u8::MAX)).unwrap();
        unsafe { transmute::<_, RingKind>(ring) }
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.0 >> u8::BITS
    }

    #[must_use]
    pub fn id(self) -> u64 {
        composite_id(self.ring(), self.index())
    }
}

impl Debug for RingAndIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingAndIndex")
            .field("ring", &self.ring())
            .field("index", &self.index())
            .finish()
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BucketAndIndex(u32);

impl BucketAndIndex {
    #[must_use]
    pub fn new(bucket: u8, index: u32) -> Self {
        debug_assert!(index <= MAX_ENTRIES);
        Self((index << u8::BITS) | u32::from(bucket))
    }

    #[must_use]
    pub fn bucket(&self) -> u8 {
        u8::try_from(self.0 & u32::from(u8::MAX)).unwrap()
    }

    #[must_use]
    pub const fn index(&self) -> u32 {
        self.0 >> u8::BITS
    }
}

impl Debug for BucketAndIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketAndIndex")
            .field("bucket", &self.bucket())
            .field("index", &self.index())
            .finish()
    }
}

mod path {
    use std::{
        fmt::{Debug, Formatter},
        ops::{Deref, DerefMut},
        path::{Path, PathBuf},
    };

    #[must_use]
    pub struct PathView<'a>(&'a mut PathBuf);

    impl<'a> PathView<'a> {
        pub fn new(path: &'a mut PathBuf, child: impl AsRef<Path>) -> Self {
            path.push(child);
            Self(path)
        }
    }

    impl Deref for PathView<'_> {
        type Target = PathBuf;

        fn deref(&self) -> &Self::Target {
            self.0
        }
    }

    impl DerefMut for PathView<'_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.0
        }
    }

    impl AsRef<Path> for PathView<'_> {
        fn as_ref(&self) -> &Path {
            self.0
        }
    }

    impl Debug for PathView<'_> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&**self, f)
        }
    }

    impl Drop for PathView<'_> {
        fn drop(&mut self) {
            self.pop();
        }
    }
}

mod string {
    use std::{
        fmt::{Debug, Formatter},
        ops::{Deref, DerefMut},
    };

    #[must_use]
    pub struct StringView<'a>(usize, &'a mut String);

    impl<'a> StringView<'a> {
        pub const fn new(s: &'a mut String) -> Self {
            Self(s.len(), s)
        }
    }

    impl Deref for StringView<'_> {
        type Target = String;

        fn deref(&self) -> &Self::Target {
            self.1
        }
    }

    impl DerefMut for StringView<'_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.1
        }
    }

    impl AsRef<str> for StringView<'_> {
        fn as_ref(&self) -> &str {
            self.1
        }
    }

    impl Debug for StringView<'_> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&**self, f)
        }
    }

    impl Drop for StringView<'_> {
        fn drop(&mut self) {
            self.1.drain(self.0..);
        }
    }
}
