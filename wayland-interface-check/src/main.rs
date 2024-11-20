#![feature(exitcode_exit_method)]

use std::{
    collections::HashSet, env, ffi::OsString, hash::BuildHasherDefault, os::unix::ffi::OsStringExt,
    process::ExitCode,
};

use rustc_hash::FxHasher;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_registry, wl_registry::WlRegistry},
};

fn main() -> ExitCode {
    let interfaces = env::args_os()
        .skip(1)
        .map(OsString::into_vec)
        .collect::<HashSet<_, _>>();
    if interfaces.is_empty() {
        return ExitCode::SUCCESS;
    }

    let Ok(conn) = Connection::connect_to_env() else {
        return ExitCode::FAILURE;
    };
    let display = conn.display();

    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = State(interfaces);

    display.get_registry(&qh, ());
    let Ok(_) = event_queue.roundtrip(&mut state) else {
        return ExitCode::FAILURE;
    };

    if state.0.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct State(HashSet<Vec<u8>, BuildHasherDefault<FxHasher>>);

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        this: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name: _,
            interface,
            version: _,
        } = event
        {
            this.0.remove(interface.as_bytes());
            if this.0.is_empty() {
                ExitCode::SUCCESS.exit_process()
            }
        }
    }
}
