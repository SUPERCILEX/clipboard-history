#!/usr/bin/gjs -m
// Smoke test for SubprocessClient. Requires a running ringboard-server.
// Run from the repo root:
//   gjs -m gnome-shell/extension/tests/test_subprocess_client.js
//
// IMPORTANT: every entry this test adds is removed before exit. The test must
// not leave artifacts in the user's clipboard history.
//
// Exit code 0 = pass, 1 = fail.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import { findBinary, SubprocessClient } from '../lib/subprocessClient.js';

let failures = 0;
function assert(cond, msg) {
  if (!cond) {
    print(`FAIL: ${msg}`);
    failures++;
  } else {
    print(`PASS: ${msg}`);
  }
}

async function main() {
  const binary = findBinary();
  assert(binary !== null, `ringboard binary discovered (got ${binary})`);
  if (!binary) return;

  const client = new SubprocessClient(binary);

  const probeOk = await client.probe();
  assert(probeOk === true, 'probe() returns true against live server');

  const sentinel = `signum-test-${Date.now()}`;
  const addedId = await client.add(sentinel);
  assert(typeof addedId === 'number' && addedId >= 0,
    `add() returned numeric id (got ${addedId})`);

  // Cleanup: ensure the sentinel does not pollute the live history. We
  // shell out directly rather than through the client because remove() is
  // not implemented in this task — it lands in the next one.
  if (typeof addedId === 'number' && addedId >= 0) {
    const proc = Gio.Subprocess.new(
      [binary, 'remove', String(addedId)],
      Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE,
    );
    proc.wait(null);
    assert(proc.get_successful(),
      `cleanup remove(${addedId}) succeeded via CLI`);
  }
}

const loop = GLib.MainLoop.new(null, false);

main().then(() => {
  if (failures > 0) {
    print(`${failures} failure(s)`);
    loop.quit();
    imports.system.exit(1);
  } else {
    print('all tests passed');
    loop.quit();
    imports.system.exit(0);
  }
}).catch(err => {
  print(`ERROR: ${err.message}\n${err.stack}`);
  loop.quit();
  imports.system.exit(1);
});

loop.run();
