import { useEffect, useRef, useState } from 'preact/hooks';

import { compactSessionId } from '../lib/format';

/** Shows a compact session id; click copies the full id (label flips to "Copied" for 1.2 s). */
export function CopyButton({ value, label = 'session ID' }: { value: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const onClick = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      clearTimeout(timer.current);
      timer.current = setTimeout(() => setCopied(false), 1_200);
    } catch {
      setCopied(false);
    }
  };

  return (
    <button
      type="button"
      class="chip-button mono"
      title={value}
      aria-label={`Copy ${label} ${value}`}
      onClick={() => void onClick()}
    >
      {copied ? 'Copied' : compactSessionId(value)}
    </button>
  );
}
