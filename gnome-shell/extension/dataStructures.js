// Helpers used by the menu render layer. The thin-UI design holds no
// in-memory entry list, so the LinkedList / LLNode types that lived here
// previously are gone.

export const MAX_VISIBLE_CHARS = 200;

// Truncate a string for single-line display in a menu item label.
// Collapses every run of whitespace (including newlines) into a single space
// so a multi-line clipboard entry renders on a single row, matching the
// behavior of the upstream gnome-clipboard-history extension. Adds an
// ellipsis when truncation occurs. `maxLen` defaults to MAX_VISIBLE_CHARS.
export function truncateLabel(text, maxLen) {
  if (typeof text !== 'string') {
    return '';
  }
  const limit = typeof maxLen === 'number' ? maxLen : MAX_VISIBLE_CHARS;
  // Single-line: collapse all whitespace runs into one space, then trim.
  const flat = text.replace(/\s+/g, ' ').trim();
  if (flat.length <= limit) {
    return flat;
  }
  return flat.slice(0, Math.max(0, limit - 1)) + '…';
}
