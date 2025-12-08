use i18n_embed::{
    DefaultLocalizer, Localizer,
    fluent::{FluentLanguageLoader, fluent_language_loader},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

static mut LANGUAGE_LOADER: Option<FluentLanguageLoader> = None;

pub fn language_loader() -> &'static FluentLanguageLoader {
    unsafe { &*&raw const LANGUAGE_LOADER }.as_ref().unwrap()
}

pub unsafe fn init() {
    unsafe {
        LANGUAGE_LOADER = Some(fluent_language_loader!());
    }

    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
    if let Err(why) =
        DefaultLocalizer::new(language_loader(), &Localizations).select(&requested_languages)
    {
        eprintln!("Failed to load fluent localizations: {why}");
    }
}

/// Request a localized string by ID from the i18n/ directory.
#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::i18n::language_loader(), $message_id)
    }};

    ($message_id:literal, $($args:expr),*) => {{
        i18n_embed_fl::fl!($crate::i18n::language_loader(), $message_id, $($args), *)
    }};
}
