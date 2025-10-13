// SPDX-License-Identifier: Apache-2.0

mod app;
mod config;
mod i18n;

fn main() -> cosmic::iced::Result {
    // Get the system's preferred languages.
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();

    // Enable localizations to be applied.
    i18n::init(&requested_languages);

    // Starts the application's event loop with `()` as the application's flags.
    cosmic::applet::run::<app::AppModel>(())
}
