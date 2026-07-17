use std::{cell::RefCell, env, ffi::OsStr};

use iced::application;

mod app;
mod message;
mod startup;
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
    let daemon = env::var_os("RINGBOARD_NO_DAEMON").is_none();
    let startup_token = if daemon && env::args_os().nth(1).as_deref() == Some(OsStr::new("toggle"))
    {
        startup::maybe_open_existing_instance_and_exit()
            .inspect_err(|e| {
                eprintln!("Failed to check for existing instance: {e}\nDetails: {e:#?}");
            })
            .ok()
    } else {
        None
    };

    let startup_token = RefCell::new(Some(startup_token));
    let result = application(
        move || RingboardApp::boot(startup_token.borrow_mut().take().flatten()),
        RingboardApp::update,
        RingboardApp::view,
    )
    .title(|app: &RingboardApp| app.title())
    .subscription(RingboardApp::subscription)
    .theme(|app: &RingboardApp| app.state.theme.theme.clone())
    .exit_on_close_request(!daemon)
    .run();

    if daemon {
        startup::cleanup();
    }
    result
}
