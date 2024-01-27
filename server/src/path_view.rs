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

impl<'a> AsRef<Path> for PathView<'a> {
    fn as_ref(&self) -> &Path {
        self.0
    }
}

impl<'a> DerefMut for PathView<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
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
