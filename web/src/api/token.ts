/**
 * The API token (`server.api_token`) the dashboard sends as `Authorization: Bearer <token>`.
 *
 * Kept in `sessionStorage` (this tab, until it closes) unless the operator ticks "remember on
 * this device", which uses `localStorage` instead. Storage access is best effort: when it is
 * blocked the token lives in memory for this page only. The token is never put in a URL; SSE
 * streams use single-use tickets instead.
 */

export const TOKEN_STORAGE_KEY = 'symphony.apiToken';

/** The subset of `Storage` used here (tests pass an in-memory one). */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export interface TokenStore {
  /** The stored token, or null. */
  get(): string | null;
  /** Saves `token` (trimmed) for this tab, or on this device when `remember` is set. */
  save(token: string, remember: boolean): void;
  /** Forgets the token everywhere. */
  clear(): void;
  /** Records that the server answered 401 to the current credentials. */
  reject(): void;
  /** True after a 401 until the next `save`/`clear`. */
  rejected(): boolean;
  subscribe(listener: () => void): () => void;
}

function read(storage: StorageLike | null): string | null {
  try {
    const value = storage?.getItem(TOKEN_STORAGE_KEY) ?? null;
    return value === null || value === '' ? null : value;
  } catch {
    return null;
  }
}

function write(storage: StorageLike | null, value: string | null): void {
  try {
    if (value === null) storage?.removeItem(TOKEN_STORAGE_KEY);
    else storage?.setItem(TOKEN_STORAGE_KEY, value);
  } catch {
    // Storage blocked (private mode, sandboxed iframe): the in-memory copy still works.
  }
}

function browserStorage(name: 'sessionStorage' | 'localStorage'): StorageLike | null {
  try {
    return typeof window === 'undefined' ? null : window[name];
  } catch {
    return null;
  }
}

/** A token store over `session` (default) and `local` (remembered) storage. */
export function createTokenStore(
  session: StorageLike | null = browserStorage('sessionStorage'),
  local: StorageLike | null = browserStorage('localStorage'),
): TokenStore {
  let token = read(session) ?? read(local);
  let wasRejected = false;
  const listeners = new Set<() => void>();
  const notify = () => {
    for (const listener of listeners) listener();
  };

  return {
    get: () => token,
    save(value, remember) {
      const trimmed = value.trim();
      token = trimmed === '' ? null : trimmed;
      wasRejected = false;
      write(remember ? local : session, token);
      write(remember ? session : local, null);
      notify();
    },
    clear() {
      token = null;
      wasRejected = false;
      write(session, null);
      write(local, null);
      notify();
    },
    reject() {
      if (wasRejected) return;
      wasRejected = true;
      notify();
    },
    rejected: () => wasRejected,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** In-memory `StorageLike` (tests, or when browser storage is unavailable). */
export function memoryStorage(initial: Record<string, string> = {}): StorageLike & {
  items: Map<string, string>;
} {
  const items = new Map(Object.entries(initial));
  return {
    items,
    getItem: (key) => items.get(key) ?? null,
    setItem: (key, value) => void items.set(key, value),
    removeItem: (key) => void items.delete(key),
  };
}
