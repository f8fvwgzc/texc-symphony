import { useState } from 'preact/hooks';

import { describeError } from '../api/client';
import { useApi } from '../api/context';

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
    <div class="refresh">
      <button type="button" class="button" disabled={busy} onClick={() => void onClick()}>
        {busy ? 'Refreshing…' : 'Refresh now'}
      </button>
      <output class={message?.error ? 'refresh-message danger' : 'refresh-message muted'}>
        {message?.text ?? ''}
      </output>
    </div>
  );
}
