import { useEffect, useState } from 'preact/hooks';

import {
  applyThemePreference,
  nextThemePreference,
  readThemePreference,
  storeThemePreference,
  type ThemePreference,
} from '../lib/theme';

const LABEL: Record<ThemePreference, string> = { system: 'Auto', light: 'Light', dark: 'Dark' };

export function ThemeToggle() {
  const [preference, setPreference] = useState<ThemePreference>(readThemePreference);
  useEffect(() => applyThemePreference(preference), [preference]);

  const onClick = () => {
    const next = nextThemePreference(preference);
    storeThemePreference(next);
    setPreference(next);
  };

  return (
    <button
      type="button"
      class="button button-ghost"
      onClick={onClick}
      aria-label={`Theme: ${LABEL[preference]}. Switch to ${LABEL[nextThemePreference(preference)]}.`}
    >
      Theme: {LABEL[preference]}
    </button>
  );
}
