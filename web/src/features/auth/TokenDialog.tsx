import { useEffect, useRef, useState } from 'preact/hooks';

import { isUnauthorized } from '../../api/client';
import { useApi } from '../../api/context';
import { Button } from '../../ui/Button';
import { ErrorText } from '../../ui/Text';

const INPUT_CLASS =
  'bg-background h-8 w-full rounded-md border px-2.5 font-mono text-sm shadow-xs outline-none ' +
  'focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px]';

/**
 * Asks for the API token (`server.api_token` / `SYMPHONY_API_TOKEN`) as a native modal that
 * cannot be dismissed: nothing works without it. The token is checked with `GET /api/v1/state`
 * before `onSaved` runs; a refused token is forgotten again.
 */
export function TokenDialog({ onSaved }: { onSaved: () => void }) {
  const api = useApi();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const [value, setValue] = useState('');
  const [remember, setRemember] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (dialog === null) return undefined;
    if (!dialog.open) {
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
    }
    const onCancel = (event: Event) => event.preventDefault();
    dialog.addEventListener('cancel', onCancel);
    return () => {
      dialog.removeEventListener('cancel', onCancel);
      if (dialog.open && typeof dialog.close === 'function') dialog.close();
    };
  }, []);

  const onSubmit = async (event: Event) => {
    event.preventDefault();
    if (value.trim() === '') {
      setError('Enter the API token.');
      return;
    }
    setBusy(true);
    setError(null);
    api.auth.save(value, remember);
    try {
      await api.getState();
    } catch (reason) {
      if (isUnauthorized(reason)) {
        api.auth.clear();
        api.auth.reject();
        setError('The server did not accept this token.');
        setBusy(false);
        return;
      }
      // Anything else (server briefly unreachable): keep the token, the live view retries.
    }
    setBusy(false);
    onSaved();
  };

  return (
    <dialog
      ref={dialogRef}
      aria-labelledby="token-dialog-title"
      aria-describedby="token-dialog-description"
      class="bg-card text-foreground m-auto w-[min(26rem,calc(100vw-2rem))] rounded-xl border p-6 shadow-lg backdrop:bg-black/40"
    >
      <form class="space-y-4" onSubmit={(event) => void onSubmit(event)}>
        <div>
          <h2 id="token-dialog-title" class="leading-none font-semibold">
            API token required
          </h2>
          <p id="token-dialog-description" class="text-muted-foreground mt-1.5 text-sm">
            This Symphony server only answers requests that carry its API token (
            <code class="font-mono">server.api_token</code> or{' '}
            <code class="font-mono">SYMPHONY_API_TOKEN</code>).
          </p>
        </div>
        <label class="block space-y-1.5 text-sm font-medium">
          <span>API token</span>
          <input
            class={INPUT_CLASS}
            type="password"
            name="token"
            autoComplete="off"
            spellcheck={false}
            value={value}
            onInput={(event) => setValue(event.currentTarget.value)}
          />
        </label>
        <label class="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={remember}
            onChange={(event) => setRemember(event.currentTarget.checked)}
          />
          Remember on this device
        </label>
        {error !== null && <ErrorText>{error}</ErrorText>}
        <div class="flex justify-end">
          <Button variant="solid" type="submit" disabled={busy}>
            {busy ? 'Checking…' : 'Connect'}
          </Button>
        </div>
      </form>
    </dialog>
  );
}
