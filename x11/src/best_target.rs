use ringboard_core::TEXT_MIMES;
use x11rb::protocol::xproto::Atom;

#[derive(Default)]
struct KnownSeenMimes {
    x_special: Option<Atom>,
    chromium_custom: Option<Atom>,
    image: Option<Atom>,
    text: Option<Atom>,
    other: Option<Atom>,
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
                    x_special,
                    chromium_custom,
                    image,
                    text,
                    other,
                },
        } = self;

        let target = if mime.starts_with("x-special/") {
            x_special
        } else if mime.starts_with("chromium/") {
            chromium_custom
        } else if mime.starts_with("image/") {
            image
        } else if TEXT_MIMES.iter().any(|b| mime.eq_ignore_ascii_case(b)) {
            text
        } else if mime.chars().next().map_or(true, char::is_lowercase) {
            other
        } else {
            return;
        };
        if target.is_none() {
            *target = Some(atom);
        }
    }

    pub fn best(&self) -> Option<Atom> {
        let Self {
            seen:
                KnownSeenMimes {
                    x_special,
                    chromium_custom,
                    image,
                    text,
                    other,
                },
        } = *self;

        x_special.or(chromium_custom).or(image).or(text).or(other)
    }
}
