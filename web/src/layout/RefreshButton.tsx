import { useState } from 'preact/hooks';

import { describeError } from '../api/client';
import { useApi } from '../api/context';
import { Button } from '../ui/Button';
import { cx } from '../ui/cx';
import { Icon } from '../ui/Icon';

/** `POST /api/v1/refresh`, then re-read the state once. */
export function RefreshButton({ onRefreshed }: { onRefreshed: () => Promise<void> | void }) {
  const api = useApi();
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ text: string; error: boolean } | null>(null);

  const onClick = async () => {
    setBusy(true);
    setMessage(null);
    try {
      const result = await api.requestRefresh();
      setMessage({
        text: result.coalesced ? 'A poll was already in progress.' : 'Poll and reconcile queued.',
        error: false,
      });
      await onRefreshed();
    } catch (error) {
      setMessage({ text: `Refresh failed (${describeError(error)})`, error: true });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="flex items-center gap-2">
      <output
        data-tone={message?.error === true ? 'danger' : 'neutral'}
        class={cx(
          'hidden text-xs lg:inline',
          message?.error === true ? 'text-destructive' : 'text-muted-foreground',
        )}
      >
        {message?.text ?? ''}
      </output>
      <Button variant="solid" disabled={busy} onClick={() => void onClick()}>
        <Icon name="refresh" />
        {busy ? 'Refreshing…' : 'Refresh now'}
      </Button>
    </div>
  );
}
