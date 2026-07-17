use iced::application;

mod app;
mod message;
mod state;
mod theme;
mod utils;
mod widgets;

use app::RingboardApp;

#[cfg(feature = "trace")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

fn main() -> iced::Result {
    application(RingboardApp::boot, RingboardApp::update, RingboardApp::view)
        .title(|app: &RingboardApp| app.title())
        .subscription(RingboardApp::subscription)
        .theme(|app: &RingboardApp| app.state.theme.theme.clone())
        .run()
}
