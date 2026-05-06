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

  // Search must find the entry we just added.
  const matches = await client.search(sentinel);
  assert(Array.isArray(matches) && matches.length >= 1,
    `search(sentinel) returned >= 1 result (got ${matches?.length})`);
  assert(matches.some(e => e.id === addedId),
    'search result includes the added entry id');
  if (matches.length > 0) {
    const e0 = matches[0];
    assert(typeof e0.id === 'number' && typeof e0.data === 'string',
      'search entries have numeric id and string data');
  }

  // moveToFront should succeed.
  const moveOk = await client.moveToFront(addedId);
  assert(moveOk === true, `moveToFront(${addedId}) returned true (got ${moveOk})`);

  // remove cleans up; verify no pollution remains.
  const removeOk = await client.remove(addedId);
  assert(removeOk === true, `remove(${addedId}) returned true (got ${removeOk})`);

  const afterRemove = await client.search(sentinel);
  assert(Array.isArray(afterRemove) && !afterRemove.some(e => e.id === addedId),
    'removed entry no longer present in search results');

  // wipe is destructive: do NOT call it. Just confirm the method exists.
  assert(typeof client.wipe === 'function', 'wipe is a function (not invoked)');
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
