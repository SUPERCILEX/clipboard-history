use ringboard_sdk::core::TEXT_MIMES;
use x11rb::protocol::xproto::Atom;

#[derive(Copy, Clone)]
struct SeenMime {
    atom: Atom,
    has_params: bool,
}

#[derive(Default)]
struct KnownSeenMimes {
    text: Option<SeenMime>,
    x_special: Option<SeenMime>,
    chromium_custom: Option<SeenMime>,
    image: Option<SeenMime>,
    other: Option<SeenMime>,
}

#[derive(Default)]
pub struct BestMimeTypeFinder {
    seen: KnownSeenMimes,
}

impl BestMimeTypeFinder {
    pub fn add_mime(&mut self, mime: &str, atom: Atom) {
        let Self {
            seen:
                KnownSeenMimes {
                    text,
                    x_special,
                    chromium_custom,
                    image,
                    other,
                },
        } = self;

        let target = if TEXT_MIMES.iter().any(|b| mime.eq_ignore_ascii_case(b)) {
            text
        } else if mime.starts_with("x-special/") {
            x_special
        } else if mime.starts_with("chromium/") {
            chromium_custom
        } else if mime.starts_with("image/") {
            image
        } else if mime.chars().next().map_or(true, char::is_lowercase) {
            other
        } else {
            return;
        };
        if target.is_none() {
            *target = Some(SeenMime {
                atom,
                has_params: mime.contains(";"),
            });
        } else if let Some(SeenMime {
            atom: _,
            has_params: true,
        }) = target
            && !mime.contains(";")
        {
            *target = Some(SeenMime {
                atom,
                has_params: false,
            });
        }
    }

    pub fn kill_text(&mut self) {
        self.seen.text = None;
    }

    pub fn best(&self) -> Option<Atom> {
        let Self {
            seen:
                KnownSeenMimes {
                    text,
                    x_special,
                    chromium_custom,
                    image,
                    other,
                },
        } = *self;

        text.or(x_special)
            .or(chromium_custom)
            .or(image)
            .or(other)
            .map(
                |SeenMime {
                     atom,
                     has_params: _,
                 }| atom,
            )
    }
}
