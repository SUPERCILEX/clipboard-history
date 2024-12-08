use std::fmt::Debug;

use ringboard_sdk::core::{is_plaintext_mime, protocol::MimeType};

#[derive(Copy, Clone, Debug)]
struct SeenMime<Id> {
    id: Id,
    has_params: bool,
}

#[derive(Default, Debug)]
struct KnownSeenMimes<Id> {
    mimes: [Option<SeenMime<Id>>; 6],
    always_none: Option<SeenMime<Id>>,
}

#[derive(Default, Debug)]
pub struct BestMimeTypeFinder<Id> {
    seen: KnownSeenMimes<Id>,
    best_mime: MimeType,
    block_plain_text: bool,
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
                    mimes:
                        [
                            ref mut plain,
                            ref mut image,
                            ref mut x_special,
                            ref mut chromium_custom,
                            ref mut any_text,
                            ref mut other,
                        ],
                    always_none: _,
                },
            ref mut best_mime,
            block_plain_text,
        } = *self;

        let target = if is_plaintext_mime(mime) {
            if block_plain_text {
                return;
            }
            plain
        } else if mime.starts_with("image/") {
            image
        } else if mime.starts_with("x-special/") {
            x_special
        } else if mime == "chromium/x-web-custom-data" {
            chromium_custom
        } else if mime.starts_with("text/") {
            any_text
        } else if mime.chars().next().is_none_or(char::is_lowercase) {
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

        if self
            .seen
            .best()
            .as_ref()
            .map(|SeenMime { id, has_params: _ }| id)
            .map(id::AsId::as_id)
            == Some(id_)
        {
            *best_mime = *mime;
        }
    }
}

impl<Id> BestMimeTypeFinder<Id> {
    pub fn block_plain_text(&mut self) {
        self.block_plain_text = true;
    }

    pub fn pop_best(&mut self) -> Option<Id> {
        self.seen
            .best()
            .take()
            .map(|SeenMime { id, has_params: _ }| id)
    }
}

impl<Id: Copy> BestMimeTypeFinder<Id> {
    pub fn best(mut self) -> Option<(Id, MimeType)> {
        (*self.seen.best()).map(|SeenMime { id, has_params: _ }| (id, self.best_mime))
    }
}

impl<Id> KnownSeenMimes<Id> {
    fn best(&mut self) -> &mut Option<SeenMime<Id>> {
        let Self { mimes, always_none } = self;
        mimes
            .iter_mut()
            .find(|m| m.is_some())
            .unwrap_or(always_none)
    }
}
