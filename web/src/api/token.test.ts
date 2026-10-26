import { describe, expect, it, vi } from 'vitest';

import { createTokenStore, memoryStorage, TOKEN_STORAGE_KEY, type StorageLike } from './token';

describe('createTokenStore', () => {
  it('keeps the token for this tab unless asked to remember it', () => {
    const session = memoryStorage();
    const local = memoryStorage();
    const store = createTokenStore(session, local);
    expect(store.get()).toBeNull();

    store.save('  abc  ', false);
    expect(store.get()).toBe('abc');
    expect(session.items.get(TOKEN_STORAGE_KEY)).toBe('abc');
    expect(local.items.has(TOKEN_STORAGE_KEY)).toBe(false);

    store.save('def', true);
    expect(local.items.get(TOKEN_STORAGE_KEY)).toBe('def');
    expect(session.items.has(TOKEN_STORAGE_KEY)).toBe(false);
  });

  it('reads an existing token, session storage first', () => {
    const local = memoryStorage({ [TOKEN_STORAGE_KEY]: 'remembered' });
    expect(createTokenStore(memoryStorage(), local).get()).toBe('remembered');
    const session = memoryStorage({ [TOKEN_STORAGE_KEY]: 'this-tab' });
    expect(createTokenStore(session, local).get()).toBe('this-tab');
  });

  it('forgets the token everywhere', () => {
    const session = memoryStorage({ [TOKEN_STORAGE_KEY]: 'a' });
    const local = memoryStorage({ [TOKEN_STORAGE_KEY]: 'b' });
    const store = createTokenStore(session, local);
    store.reject();
    store.clear();
    expect(store.get()).toBeNull();
    expect(store.rejected()).toBe(false);
    expect(session.items.size).toBe(0);
    expect(local.items.size).toBe(0);
  });

  it('tracks rejection and notifies subscribers', () => {
    const store = createTokenStore(memoryStorage(), memoryStorage());
    const listener = vi.fn();
    const unsubscribe = store.subscribe(listener);
    store.reject();
    store.reject();
    expect(store.rejected()).toBe(true);
    expect(listener).toHaveBeenCalledTimes(1);
    store.save('x', false);
    expect(store.rejected()).toBe(false);
    expect(listener).toHaveBeenCalledTimes(2);
    unsubscribe();
    store.clear();
    expect(listener).toHaveBeenCalledTimes(2);
  });

  it('survives blocked storage', () => {
    const blocked: StorageLike = {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
      removeItem: () => {
        throw new Error('denied');
      },
    };
    const store = createTokenStore(blocked, blocked);
    expect(store.get()).toBeNull();
    store.save('in-memory', true);
    expect(store.get()).toBe('in-memory');
    store.clear();
    expect(store.get()).toBeNull();
    expect(createTokenStore(null, null).get()).toBeNull();
  });
});
