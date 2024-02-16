pub use path::PathView;
pub use string::StringView;

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

    impl<'a> Deref for PathView<'a> {
        type Target = PathBuf;

        fn deref(&self) -> &Self::Target {
            self.0
        }
    }

    impl<'a> DerefMut for PathView<'a> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.0
        }
    }

    impl<'a> AsRef<Path> for PathView<'a> {
        fn as_ref(&self) -> &Path {
            self.0
        }
    }

    impl<'a> Debug for PathView<'a> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&**self, f)
        }
    }

    impl<'a> Drop for PathView<'a> {
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
        pub fn new(s: &'a mut String) -> Self {
            Self(s.len(), s)
        }
    }

    impl<'a> Deref for StringView<'a> {
        type Target = String;

        fn deref(&self) -> &Self::Target {
            self.1
        }
    }

    impl<'a> DerefMut for StringView<'a> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            self.1
        }
    }

    impl<'a> AsRef<str> for StringView<'a> {
        fn as_ref(&self) -> &str {
            self.1
        }
    }

    impl<'a> Debug for StringView<'a> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&**self, f)
        }
    }

    impl<'a> Drop for StringView<'a> {
        fn drop(&mut self) {
            self.1.drain(self.0..);
        }
    }
}
