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
