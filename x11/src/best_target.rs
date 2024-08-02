use ringboard_sdk::core::{protocol::MimeType, TEXT_MIMES};
use x11rb::protocol::xproto::Atom;

#[derive(Copy, Clone)]
struct SeenMime {
    atom: Atom,
    has_params: bool,
}

#[derive(Default)]
struct KnownSeenMimes {
    text: Option<SeenMime>,
    image: Option<SeenMime>,
    x_special: Option<SeenMime>,
    chromium_custom: Option<SeenMime>,
    other: Option<SeenMime>,
}

#[derive(Default)]
pub struct BestMimeTypeFinder {
    seen: KnownSeenMimes,
    best_mime: MimeType,
    block_text: bool,
}

impl BestMimeTypeFinder {
    pub fn add_mime(&mut self, mime: &MimeType, atom: Atom) {
        let Self {
            seen:
                KnownSeenMimes {
                    text,
                    image,
                    x_special,
                    chromium_custom,
                    other,
                },
            best_mime,
            block_text,
        } = self;

        let target = if TEXT_MIMES.iter().any(|b| mime.eq_ignore_ascii_case(b)) {
            if *block_text {
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
        if target.is_none() {
            *target = Some(SeenMime {
                atom,
                has_params: mime.contains(';'),
            });
        } else if let Some(SeenMime {
            atom: _,
            has_params: true,
        }) = target
            && !mime.contains(';')
        {
            *target = Some(SeenMime {
                atom,
                has_params: false,
            });
        }

        if self.seen.best() == Some(atom) {
            *best_mime = *mime;
        }
    }

    pub fn block_text(&mut self) {
        self.block_text = true;
    }

    pub fn best(&self) -> Option<(Atom, MimeType)> {
        self.seen.best().map(|atom| (atom, self.best_mime))
    }
}

impl KnownSeenMimes {
    fn best(&self) -> Option<Atom> {
        let Self {
            text,
            image,
            x_special,
            chromium_custom,
            other,
        } = *self;

        text.or(image)
            .or(x_special)
            .or(chromium_custom)
            .or(other)
            .map(
                |SeenMime {
                     atom,
                     has_params: _,
                 }| atom,
            )
    }
}
