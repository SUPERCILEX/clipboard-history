import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { selectBestMime } from './mimePriority.js';

test('empty input returns null', () => {
  assert.equal(selectBestMime([]), null);
});

test('non-array input returns null', () => {
  assert.equal(selectBestMime(null), null);
  assert.equal(selectBestMime(undefined), null);
});

test('text/plain only → plain text slot', () => {
  assert.deepEqual(selectBestMime(['text/plain']), { mime: 'text/plain', isText: true });
});

test('plain-text aliases (case-insensitive)', () => {
  for (const m of ['', 'TEXT', 'STRING', 'UTF8_STRING', 'text/plain;charset=utf-8']) {
    const got = selectBestMime([m]);
    assert.deepEqual(got, { mime: m, isText: true }, `failed for ${JSON.stringify(m)}`);
  }
});

test('image/png only → image slot, isText false', () => {
  assert.deepEqual(selectBestMime(['image/png']), { mime: 'image/png', isText: false });
});

test('plain text beats image when both are offered', () => {
  assert.deepEqual(
    selectBestMime(['image/png', 'text/plain']),
    { mime: 'text/plain', isText: true },
  );
});

test('image beats x-special', () => {
  assert.deepEqual(
    selectBestMime(['x-special/gnome-copied-files', 'image/png']),
    { mime: 'image/png', isText: false },
  );
});

test('x-special beats chromium custom', () => {
  assert.deepEqual(
    selectBestMime(['chromium/x-web-custom-data', 'x-special/foo']),
    { mime: 'x-special/foo', isText: false },
  );
});

test('any text/* falls into the any-text slot when no plain text', () => {
  assert.deepEqual(
    selectBestMime(['text/html']),
    { mime: 'text/html', isText: false },
  );
});

test('plain text beats text/html', () => {
  assert.deepEqual(
    selectBestMime(['text/html', 'text/plain']),
    { mime: 'text/plain', isText: true },
  );
});
