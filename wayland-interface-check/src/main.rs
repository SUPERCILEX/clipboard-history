#![feature(exitcode_exit_method)]

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    hash::BuildHasherDefault,
    os::unix::ffi::OsStringExt,
    process::ExitCode,
};

use rustc_hash::FxHasher;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{wl_registry, wl_registry::WlRegistry},
};

fn main() -> ExitCode {
    let mut verbose = false;
    let interfaces = env::args_os()
        .skip(1)
        .filter(|arg| {
            if arg == OsStr::new("--verbose") {
                verbose = true;
                false
            } else {
                true
            }
        })
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

    let mut state = State {
        verbose,
        interfaces,
    };

    display.get_registry(&qh, ());
    let Ok(_) = event_queue.roundtrip(&mut state) else {
        return ExitCode::FAILURE;
    };

    if state.interfaces.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct State {
    verbose: bool,
    interfaces: HashSet<Vec<u8>, BuildHasherDefault<FxHasher>>,
}

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
            version,
        } = event
        {
            if this.verbose {
                println!("{interface}:v{version}");
            }
            this.interfaces.remove(interface.as_bytes());
            if this.interfaces.is_empty() {
                ExitCode::SUCCESS.exit_process()
            }
        }
    }
}
