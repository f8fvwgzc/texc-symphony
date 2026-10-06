import { useEffect, useState } from 'preact/hooks';

import {
  applyThemePreference,
  nextThemePreference,
  readThemePreference,
  storeThemePreference,
  type ThemePreference,
} from '../lib/theme';
import { Button } from '../ui/Button';
import { Icon } from '../ui/Icon';

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
    <Button
      onClick={onClick}
      aria-label={`Theme: ${LABEL[preference]}. Switch to ${LABEL[nextThemePreference(preference)]}.`}
    >
      <Icon name="theme" />
      Theme: {LABEL[preference]}
    </Button>
  );
}
