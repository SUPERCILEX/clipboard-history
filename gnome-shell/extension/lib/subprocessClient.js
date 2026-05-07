import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

// Locate the `ringboard` CLI by walking $PATH. Returns the absolute path of
// the first executable match, or null if nothing is found.
export function findBinary(name = 'ringboard') {
  const pathEnv = GLib.getenv('PATH') || '';
  for (const dir of pathEnv.split(':')) {
    if (!dir) continue;
    const candidate = GLib.build_filenamev([dir, name]);
    if (GLib.file_test(candidate, GLib.FileTest.IS_EXECUTABLE)) {
      return candidate;
    }
  }
  return null;
}

// Async wrapper around `ringboard` CLI. All methods return Promises and rely
// on Gio.Subprocess.communicate_utf8_async, which dispatches I/O on a worker
// thread. The GNOME Shell main loop is never blocked.
export class SubprocessClient {
  constructor(binaryPath) {
    if (!binaryPath) {
      throw new Error('SubprocessClient: binary path required');
    }
    this._binary = binaryPath;
  }

  // Run a subprocess that does not need stdin. Resolves with
  // { ok: bool, stdout: string, stderr: string, exit: number }.
  _run(argv, stdin) {
    return new Promise((resolve, reject) => {
      let proc;
      try {
        const flags =
          Gio.SubprocessFlags.STDOUT_PIPE |
          Gio.SubprocessFlags.STDERR_PIPE |
          (stdin != null ? Gio.SubprocessFlags.STDIN_PIPE : 0);
        proc = Gio.Subprocess.new([this._binary, ...argv], flags);
      } catch (e) {
        reject(e);
        return;
      }
      proc.communicate_utf8_async(stdin ?? null, null, (p, res) => {
        try {
          const [, stdout, stderr] = p.communicate_utf8_finish(res);
          const ok = p.get_successful();
          const exit = p.get_exit_status();
          resolve({ ok, stdout: stdout ?? '', stderr: stderr ?? '', exit });
        } catch (e) {
          reject(e);
        }
      });
    });
  }

  // Connectivity check. `ringboard last` reads from the server socket; if the
  // server is down it exits non-zero. We deliberately silence stdout because
  // `last` returns the raw bytes of the newest entry, which may be a binary
  // payload (image, etc.) that would fail UTF-8 decoding in _run.
  async probe() {
    return new Promise(resolve => {
      let proc;
      try {
        proc = Gio.Subprocess.new(
          [this._binary, 'last'],
          Gio.SubprocessFlags.STDOUT_SILENCE | Gio.SubprocessFlags.STDERR_SILENCE,
        );
      } catch (_) {
        resolve(false);
        return;
      }
      proc.wait_async(null, (p, res) => {
        try {
          p.wait_finish(res);
          resolve(p.get_successful());
        } catch (_) {
          resolve(false);
        }
      });
    });
  }

  // Submit a text entry. Returns the numeric id assigned by the server, or
  // null on failure. The CLI prints the id to stdout on success.
  async add(text) {
    if (typeof text !== 'string' || text.length === 0) {
      return null;
    }
    let r;
    try {
      r = await this._run(['add', '-'], text);
    } catch (_) {
      return null;
    }
    if (!r.ok) {
      return null;
    }
    // CLI outputs e.g. "Entry added: 4294979852"
    const match = r.stdout.match(/:\s*(\d+)/);
    if (!match) return null;
    const id = Number.parseInt(match[1], 10);
    return Number.isFinite(id) ? id : null;
  }

  // Search for entries. Empty query returns all *text* entries newest-first
  // (the CLI's search command is text-only). For unfiltered listing including
  // binary/image entries, use dump().
  // Returns an array of { id, kind, data } objects.
  async search(query) {
    const q = typeof query === 'string' ? query : '';
    let r;
    try {
      r = await this._run(['search', '--json', q]);
    } catch (_) {
      return [];
    }
    if (!r.ok) {
      return [];
    }
    const text = r.stdout.trim();
    if (text.length === 0) {
      return [];
    }
    try {
      const parsed = JSON.parse(text);
      if (!Array.isArray(parsed)) return [];
      return parsed.filter(e =>
        e && typeof e.id === 'number' && typeof e.data === 'string'
      );
    } catch (_) {
      return [];
    }
  }

  // Dump every entry (text + binary) via `ringboard debug dump`. Returns an
  // array of { id, kind, mime_type, data } objects where data is UTF-8 text
  // for kind === "Human" and base64-encoded bytes for kind === "Bytes".
  // Used by the menu's empty-query path so image entries are visible.
  async dump() {
    let r;
    try {
      r = await this._run(['debug', 'dump']);
    } catch (_) {
      return [];
    }
    if (!r.ok) {
      return [];
    }
    const text = r.stdout.trim();
    if (text.length === 0) {
      return [];
    }
    try {
      const parsed = JSON.parse(text);
      if (!Array.isArray(parsed)) return [];
      const filtered = parsed.filter(e =>
        e && typeof e.id === 'number' && typeof e.kind === 'string'
      );
      // dump emits oldest-first; the menu wants newest-first to match search.
      filtered.reverse();
      return filtered;
    } catch (_) {
      return [];
    }
  }

  // Move an entry to the front of the ring. Returns true on success.
  async moveToFront(id) {
    if (!Number.isFinite(id)) return false;
    try {
      const r = await this._run(['move-to-front', String(id)]);
      return r.ok;
    } catch (_) {
      return false;
    }
  }

  // Remove an entry by id. Returns true on success.
  async remove(id) {
    if (!Number.isFinite(id)) return false;
    try {
      const r = await this._run(['remove', String(id)]);
      return r.ok;
    } catch (_) {
      return false;
    }
  }

  // Wipe the entire history. Returns true on success.
  // DO NOT call this from automated tests; it is destructive.
  async wipe() {
    try {
      const r = await this._run(['wipe']);
      return r.ok;
    } catch (_) {
      return false;
    }
  }
}
