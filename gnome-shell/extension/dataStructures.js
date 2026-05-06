// Helpers used by the menu render layer. The thin-UI design holds no
// in-memory entry list, so the LinkedList / LLNode types that lived here
// previously are gone.

export const MAX_VISIBLE_CHARS = 200;

// Truncate a string for display in a menu item label. Adds an ellipsis when
// truncation occurs. `maxLen` defaults to MAX_VISIBLE_CHARS.
export function truncateLabel(text, maxLen) {
  if (typeof text !== 'string') {
    return '';
  }
  const limit = typeof maxLen === 'number' ? maxLen : MAX_VISIBLE_CHARS;
  if (text.length <= limit) {
    return text;
  }
  return text.slice(0, Math.max(0, limit - 1)) + '…';
}
