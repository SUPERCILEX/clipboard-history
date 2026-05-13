#![cfg(feature = "dbus")]

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use tokio::time::sleep;
use zbus::{Connection, Proxy, connection::Builder};

const BUS_NAME: &str = "com.github.SUPERCILEX.Ringboard";
const OBJECT_PATH: &str = "/com/github/SUPERCILEX/Ringboard";
const INTERFACE: &str = "com.github.SUPERCILEX.Ringboard1";

fn binary_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push("target");
    p.push(if cfg!(debug_assertions) { "debug" } else { "release" });
    p.push("ringboard-server");
    p
}

struct Bus {
    addr: String,
    pid: u32,
    _server: Child,
    _tmpdir: tempfile::TempDir,
}

fn start_bus_and_server() -> Option<Bus> {
    if which::which("dbus-launch").is_err() {
        return None;
    }
    let output = Command::new("dbus-launch").arg("--sh-syntax").output().ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut addr = None;
    let mut pid = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_ADDRESS='") {
            addr = rest.strip_suffix("';").map(|s| s.to_owned());
        } else if let Some(rest) = line.strip_prefix("DBUS_SESSION_BUS_PID=") {
            pid = rest.trim_end_matches(';').parse::<u32>().ok();
        }
    }
    let addr = addr?;
    let pid = pid?;

    let tmpdir = tempfile::tempdir().ok()?;
    let server = Command::new(binary_path())
        .env("XDG_DATA_HOME", tmpdir.path())
        .env("DBUS_SESSION_BUS_ADDRESS", &addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    Some(Bus { addr, pid, _server: server, _tmpdir: tmpdir })
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self._server.kill();
        let _ = Command::new("kill").arg(self.pid.to_string()).status();
    }
}

async fn proxy(addr: &str) -> Proxy<'static> {
    let conn: Connection = Builder::address(addr)
        .unwrap()
        .build()
        .await
        .unwrap();
    Proxy::new_owned(conn, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn add_search_remove_roundtrip() {
    let Some(bus) = start_bus_and_server() else {
        eprintln!("dbus-launch not available; skipping");
        return;
    };
    // Give the server a moment to register on the bus.
    sleep(Duration::from_millis(500)).await;
    let p = proxy(&bus.addr).await;

    let id: u64 = p
        .call("Add", &(b"hello".as_slice(), "text/plain"))
        .await
        .unwrap();

    let (page, total): (Vec<(u64, String, Vec<u8>)>, u64) =
        p.call("Search", &("", 0u64, 50u64)).await.unwrap();
    assert_eq!(total, 1, "total should be 1");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].0, id);
    assert_eq!(page[0].2, b"hello");

    let (page2, _): (Vec<(u64, String, Vec<u8>)>, u64) =
        p.call("Search", &("ell", 0u64, 50u64)).await.unwrap();
    assert_eq!(page2.len(), 1, "text search should match");

    p.call::<_, (u64,), ()>("Remove", &(id,)).await.unwrap();

    let (_, total_after): (Vec<(u64, String, Vec<u8>)>, u64) =
        p.call("Search", &("", 0u64, 50u64)).await.unwrap();
    assert_eq!(total_after, 0);
}
