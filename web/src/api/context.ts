import { createContext } from 'preact';
import { useContext } from 'preact/hooks';

import { createApiClient, type ApiClient } from './client';

/** API client used by every component (tests provide a fake through `ApiContext.Provider`). */
export const ApiContext = createContext<ApiClient>(createApiClient());

export function useApi(): ApiClient {
  return useContext(ApiContext);
}
