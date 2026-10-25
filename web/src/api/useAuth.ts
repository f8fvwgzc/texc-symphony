import { useEffect, useState } from 'preact/hooks';

import type { TokenStore } from './token';

export interface AuthView {
  /** The stored API token, or null. */
  token: string | null;
  /** The server answered 401 since the token was last saved or cleared. */
  rejected: boolean;
}

/** Re-renders whenever `store` changes. */
export function useAuth(store: TokenStore): AuthView {
  const [, setVersion] = useState(0);
  useEffect(() => store.subscribe(() => setVersion((value) => value + 1)), [store]);
  return { token: store.get(), rejected: store.rejected() };
}
