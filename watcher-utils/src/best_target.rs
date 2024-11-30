use std::fmt::Debug;

use ringboard_sdk::core::{is_plaintext_mime, protocol::MimeType};

#[derive(Debug)]
struct SeenMime<Id> {
    id: Id,
    has_params: bool,
}

#[derive(Default, Debug)]
struct KnownSeenMimes<Id> {
    text: Option<SeenMime<Id>>,
    image: Option<SeenMime<Id>>,
    x_special: Option<SeenMime<Id>>,
    chromium_custom: Option<SeenMime<Id>>,
    other: Option<SeenMime<Id>>,
}

#[derive(Default, Debug)]
pub struct BestMimeTypeFinder<Id> {
    seen: KnownSeenMimes<Id>,
    best_mime: MimeType,
    block_text: bool,
}

mod id {
    pub trait AsId {
        type Id;

        fn as_id(&self) -> Self::Id;
    }

    impl AsId for String {
        type Id = *const u8;

        fn as_id(&self) -> Self::Id {
            self.as_ptr()
        }
    }

    impl AsId for u32 {
        type Id = Self;

        fn as_id(&self) -> Self::Id {
            *self
        }
    }
}

impl<Id: id::AsId<Id: Eq>> BestMimeTypeFinder<Id> {
    pub fn add_mime(&mut self, mime: &MimeType, id: Id) {
        let Self {
            seen:
                KnownSeenMimes {
                    ref mut text,
                    ref mut image,
                    ref mut x_special,
                    ref mut chromium_custom,
                    ref mut other,
                },
            ref mut best_mime,
            block_text,
        } = *self;

        let target = if is_plaintext_mime(mime) {
            if block_text {
                return;
            }
            text
        } else if mime.starts_with("image/") {
            image
        } else if mime.starts_with("x-special/") {
            x_special
        } else if mime.starts_with("chromium/") {
            chromium_custom
        } else if mime.chars().next().map_or(true, char::is_lowercase) {
            other
        } else {
            return;
        };
        let id_ = id.as_id();
        if target.is_none() {
            *target = Some(SeenMime {
                id,
                has_params: mime.contains(';'),
            });
        } else if let Some(SeenMime {
            id: _,
            has_params: true,
        }) = target
            && !mime.contains(';')
        {
            *target = Some(SeenMime {
                id,
                has_params: false,
            });
        }

        if self.seen.best().map(id::AsId::as_id) == Some(id_) {
            *best_mime = *mime;
        }
    }

    pub fn block_text(&mut self) {
        self.block_text = true;
    }

    pub fn best(self) -> Option<(Id, MimeType)> {
        self.seen.into_best().map(|id| (id, self.best_mime))
    }
}

impl<Id> KnownSeenMimes<Id> {
    fn best(&self) -> Option<&Id> {
        let Self {
            text,
            image,
            x_special,
            chromium_custom,
            other,
        } = self;

        text.as_ref()
            .or(image.as_ref())
            .or(x_special.as_ref())
            .or(chromium_custom.as_ref())
            .or(other.as_ref())
            .map(|SeenMime { id, has_params: _ }| id)
    }

    fn into_best(self) -> Option<Id> {
        let Self {
            text,
            image,
            x_special,
            chromium_custom,
            other,
        } = self;

        text.or(image)
            .or(x_special)
            .or(chromium_custom)
            .or(other)
            .map(|SeenMime { id, has_params: _ }| id)
    }
}
