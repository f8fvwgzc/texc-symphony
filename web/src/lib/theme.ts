/** Light / dark / follow-the-OS theme, remembered per browser (best effort). */
export type ThemePreference = 'system' | 'light' | 'dark';

const STORAGE_KEY = 'symphony.theme';

export function readThemePreference(): ThemePreference {
  try {
    const value = window.localStorage.getItem(STORAGE_KEY);
    return value === 'light' || value === 'dark' ? value : 'system';
  } catch {
    return 'system';
  }
}

export function storeThemePreference(preference: ThemePreference): void {
  try {
    if (preference === 'system') window.localStorage.removeItem(STORAGE_KEY);
    else window.localStorage.setItem(STORAGE_KEY, preference);
  } catch {
    // Storage blocked (private mode, sandboxed iframe): the choice lasts for this page only.
  }
}

/** `system` removes the attribute so the `prefers-color-scheme` media query decides. */
export function applyThemePreference(preference: ThemePreference, root = document.documentElement) {
  if (preference === 'system') delete root.dataset['theme'];
  else root.dataset['theme'] = preference;
}

export function nextThemePreference(current: ThemePreference): ThemePreference {
  return current === 'system' ? 'light' : current === 'light' ? 'dark' : 'system';
}
