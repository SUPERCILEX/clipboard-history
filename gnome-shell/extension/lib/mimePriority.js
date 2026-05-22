// Mirrors `BestMimeTypeFinder` from
// client-sdk/src/watcher_utils/best_target.rs. Given the MIME types a
// clipboard source has advertised, return the single MIME we want to
// capture, or null if the offer should be dropped (passwords, unknown
// app-internal targets, empty).
//
// Slot priority (highest first):
//   0  plain text  (is_plaintext_mime: '', text, string, utf8_string,
//                   text/plain, text/plain;charset=*)
//   1  image/*
//   2  x-special/*
//   3  chromium/x-web-custom-data
//   4  any other text/*
//   5  anything else starting with an ASCII lowercase letter

const PLAIN_TEXT_ALIASES = new Set([
  '',
  'text',
  'string',
  'utf8_string',
  'text/plain',
  'text/plain;charset=utf-8',
  'text/plain;charset=us-ascii',
  'text/plain;charset=unicode',
]);

const SLOT_PLAIN = 0;
const SLOT_IMAGE = 1;
const SLOT_X_SPECIAL = 2;
const SLOT_CHROMIUM_CUSTOM = 3;
const SLOT_ANY_TEXT = 4;
const SLOT_OTHER = 5;
const NUM_SLOTS = 6;

function isPlaintextMime(mime) {
  return PLAIN_TEXT_ALIASES.has(mime.toLowerCase());
}

function startsWithLowercaseAscii(s) {
  if (s.length === 0) return true;
  const c = s.charCodeAt(0);
  return c >= 0x61 && c <= 0x7a; // 'a'..'z'
}

// Classify one MIME into a slot, or one of the sentinel strings:
//   'skip'     — ignore this MIME, keep processing others
//   'password' — privacy hint seen, drop the whole entry
function classify(mime) {
  if (isPlaintextMime(mime)) return SLOT_PLAIN;
  if (mime.startsWith('image/')) return SLOT_IMAGE;
  if (mime.startsWith('x-special/')) return SLOT_X_SPECIAL;
  if (mime === 'chromium/x-web-custom-data') return SLOT_CHROMIUM_CUSTOM;
  if (mime.startsWith('chromium/x-internal')) return 'skip';
  if (mime.startsWith('text/')) return SLOT_ANY_TEXT;
  if (mime === 'x-kde-passwordManagerHint') return 'password';
  if (startsWithLowercaseAscii(mime)) return SLOT_OTHER;
  return 'skip';
}

export function selectBestMime(mimes) {
  if (!Array.isArray(mimes)) return null;

  const slots = new Array(NUM_SLOTS).fill(null);
  let isPassword = false;

  for (const mime of mimes) {
    if (typeof mime !== 'string') continue;
    const cls = classify(mime);
    if (cls === 'skip') continue;
    if (cls === 'password') {
      isPassword = true;
      continue;
    }
    const current = slots[cls];
    if (current === null) {
      slots[cls] = mime;
    } else if (current.includes(';') && !mime.includes(';')) {
      // Prefer the MIME without parameters within the same slot.
      slots[cls] = mime;
    }
  }

  if (isPassword) return null;

  for (let s = 0; s < NUM_SLOTS; s++) {
    if (slots[s] !== null) {
      return { mime: slots[s], isText: s === SLOT_PLAIN };
    }
  }
  return null;
}
