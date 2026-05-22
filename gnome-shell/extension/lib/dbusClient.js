import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

// Talks to ringboard-server's DBus interface on the session bus. Mirrors
// the legacy SubprocessClient API so menuController / clipboardIntake
// don't need to change shape.
//
// Server-side surface (see server/src/dbus.rs):
//   com.github.SUPERCILEX.Ringboard1.Add(ay payload, s mime) -> t id
//   com.github.SUPERCILEX.Ringboard1.Search(s query, t offset, t limit)
//       -> (a(tsay) page, t total)
//   com.github.SUPERCILEX.Ringboard1.MoveToFront(t id) -> ()
//   com.github.SUPERCILEX.Ringboard1.Remove(t id) -> ()
//   com.github.SUPERCILEX.Ringboard1.Wipe() -> ()

const BUS_NAME = 'com.github.SUPERCILEX.Ringboard';
const OBJECT_PATH = '/com/github/SUPERCILEX/Ringboard';
const IFACE = 'com.github.SUPERCILEX.Ringboard1';

// Match the search page limit the server applies.
const PAGE_LIMIT = 500;

const TEXT_DECODER = new TextDecoder('utf-8', { fatal: false });

// Convert raw entry bytes + MIME into the legacy `{id, kind, data, mime_type}`
// shape the menu consumes. Text entries get `kind: 'Human'` with utf-8 data;
// non-text entries get `kind: 'Bytes'` with base64-encoded data so the menu
// can render thumbnails by GLib.base64_decode-ing the field, the same path it
// took when consuming `ringboard debug dump --json` output.
// Mirror ringboard-sdk's is_text_mime: empty / "text" / text/* / a handful
// of JSON+XML application subtypes count as human-readable. Old watchers
// stored entries with empty mime, so without the empty-string case we'd
// render every existing entry as binary.
function isTextMime(mime) {
  if (typeof mime !== 'string') return false;
  if (mime.length === 0) return true;
  if (mime === 'text' || mime.startsWith('text/')) return true;
  return mime === 'application/json' || mime === 'application/xml';
}

function rowToEntry([id, mime, payloadBytes]) {
  const bytes = payloadBytes instanceof Uint8Array
    ? payloadBytes
    : new Uint8Array(payloadBytes);
  if (isTextMime(mime)) {
    return {
      id: Number(id),
      kind: 'Human',
      data: TEXT_DECODER.decode(bytes),
      mime_type: mime || 'text/plain',
    };
  }
  return {
    id: Number(id),
    kind: 'Bytes',
    data: GLib.base64_encode(bytes),
    mime_type: mime || 'application/octet-stream',
  };
}

export class DbusClient {
  constructor() {
    this._conn = Gio.bus_get_sync(Gio.BusType.SESSION, null);
  }

  // Wrap Gio.DBusConnection.call into a Promise.
  _call(method, params, replyType) {
    return new Promise((resolve, reject) => {
      this._conn.call(
        BUS_NAME,
        OBJECT_PATH,
        IFACE,
        method,
        params,
        replyType,
        Gio.DBusCallFlags.NONE,
        -1,
        null,
        (conn, res) => {
          try {
            const reply = conn.call_finish(res);
            resolve(reply);
          } catch (e) {
            reject(e);
          }
        },
      );
    });
  }

  // True if the ringboard bus name has an owner. Used at startup to decide
  // between the populated menu and the "server unavailable" placeholder.
  async probe() {
    try {
      const reply = await new Promise((resolve, reject) => {
        this._conn.call(
          'org.freedesktop.DBus',
          '/org/freedesktop/DBus',
          'org.freedesktop.DBus',
          'NameHasOwner',
          new GLib.Variant('(s)', [BUS_NAME]),
          new GLib.VariantType('(b)'),
          Gio.DBusCallFlags.NONE,
          -1,
          null,
          (conn, res) => {
            try { resolve(conn.call_finish(res)); }
            catch (e) { reject(e); }
          },
        );
      });
      const [owned] = reply.deep_unpack();
      return Boolean(owned);
    } catch (_) {
      return false;
    }
  }

  // Submit an entry. Returns the numeric id assigned by the server, or
  // null on failure. `payloadBytes` is a Uint8Array; `mime` is the MIME
  // type to record (e.g. 'text/plain;charset=utf-8' or 'image/png').
  async add(payloadBytes, mime) {
    if (!(payloadBytes instanceof Uint8Array) || payloadBytes.length === 0) return null;
    if (typeof mime !== 'string' || mime.length === 0) return null;
    try {
      const reply = await this._call(
        'Add',
        new GLib.Variant('(ays)', [payloadBytes, mime]),
        new GLib.VariantType('(t)'),
      );
      const [id] = reply.deep_unpack();
      return Number(id);
    } catch (e) {
      console.warn(`ringboard: dbus Add failed: ${e.message}`);
      return null;
    }
  }

  // Text search. Returns an array of `{id, kind, data, mime_type}` entries
  // matching the query, capped at PAGE_LIMIT rows.
  async search(query) {
    return this._search(typeof query === 'string' ? query : '');
  }

  // Full listing (text + binary), newest-first. The server's Search method
  // returns everything when query is empty, so dump() and search('') are the
  // same call — kept as a separate name only to match the legacy API.
  async dump() {
    return this._search('');
  }

  async _search(query) {
    try {
      const reply = await this._call(
        'Search',
        new GLib.Variant('(stt)', [query, 0n, BigInt(PAGE_LIMIT)]),
        new GLib.VariantType('(a(tsay)t)'),
      );
      const [page] = reply.deep_unpack();
      return page.map(rowToEntry);
    } catch (e) {
      console.warn(`ringboard: dbus Search failed: ${e.message}`);
      return [];
    }
  }

  async moveToFront(id) {
    if (!Number.isFinite(id)) return false;
    try {
      await this._call(
        'MoveToFront',
        new GLib.Variant('(t)', [BigInt(id)]),
        null,
      );
      return true;
    } catch (e) {
      console.warn(`ringboard: dbus MoveToFront failed: ${e.message}`);
      return false;
    }
  }

  async remove(id) {
    if (!Number.isFinite(id)) return false;
    try {
      await this._call(
        'Remove',
        new GLib.Variant('(t)', [BigInt(id)]),
        null,
      );
      return true;
    } catch (e) {
      console.warn(`ringboard: dbus Remove failed: ${e.message}`);
      return false;
    }
  }

  async wipe() {
    try {
      await this._call('Wipe', null, null);
      return true;
    } catch (e) {
      console.warn(`ringboard: dbus Wipe failed: ${e.message}`);
      return false;
    }
  }
}
