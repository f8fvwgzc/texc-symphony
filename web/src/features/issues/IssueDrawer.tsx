import { useEffect, useRef } from 'preact/hooks';

import { ApiError, describeError } from '../../api/client';
import { useApi } from '../../api/context';
import type { IssueStatus } from '../../api/types';
import { useAsync } from '../../hooks/useAsync';
import { useThrottled } from '../../hooks/useThrottled';
import type { Tone } from '../../lib/format';
import { Badge } from '../../ui/Badge';
import { Button } from '../../ui/Button';
import { Icon } from '../../ui/Icon';
import { Empty, ErrorText } from '../../ui/Text';
import { IssueBody } from './IssueBody';
import { IssueId } from './IssueId';
import { RecentRuns } from './RecentRuns';

const STATUS_TONE: Record<IssueStatus, Tone> = {
  running: 'active',
  retrying: 'warning',
  blocked: 'danger',
};

/**
 * Opens `dialog` as a native modal (focus trap, Escape and focus restore come for free) and calls
 * `onClose` on Escape or on a click on the backdrop.
 */
function useModalDialog(onClose: () => void) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = dialogRef.current;
    if (dialog === null) return undefined;
    if (!dialog.open) {
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
    }
    const onCancel = (event: Event) => {
      event.preventDefault();
      onClose();
    };
    // A click whose target is the <dialog> itself hit the backdrop.
    const onBackdropClick = (event: MouseEvent) => {
      if (event.target === dialog) onClose();
    };
    dialog.addEventListener('cancel', onCancel);
    dialog.addEventListener('click', onBackdropClick);
    return () => {
      dialog.removeEventListener('cancel', onCancel);
      dialog.removeEventListener('click', onBackdropClick);
      if (dialog.open && typeof dialog.close === 'function') dialog.close();
    };
  }, [onClose]);
  return dialogRef;
}

/**
 * Side panel with `GET /api/v1/{identifier}`. Re-fetched (throttled to every 2 s) whenever
 * the live state changes. Escape, the close button or a click on the backdrop close it.
 */
export function IssueDrawer(props: {
  identifier: string;
  issueUrl: string | null;
  revision: number;
  storeEnabled: boolean;
  now: number;
  onClose: () => void;
}) {
  const api = useApi();
  const dialogRef = useModalDialog(props.onClose);
  const revision = useThrottled(props.revision, 2_000);

  const detail = useAsync(
    (signal) => api.getIssue(props.identifier, { signal }),
    [api, props.identifier, revision],
  );
  const runs = useAsync(
    (signal) =>
      props.storeEnabled
        ? api.listRuns({ issue: props.identifier, limit: 5 }, { signal })
        : Promise.resolve(null),
    [api, props.identifier, props.storeEnabled],
  );

  const notFound = detail.error instanceof ApiError && detail.error.code === 'issue_not_found';

  return (
    <dialog
      class="bg-background text-foreground m-0 ml-auto h-dvh max-h-none w-full max-w-xl border-l p-0 shadow-lg"
      aria-labelledby="drawer-title"
      ref={dialogRef}
    >
      <div class="flex h-full flex-col">
        <header class="flex items-start justify-between gap-4 border-b px-5 py-4">
          <div class="space-y-1">
            <p class="text-muted-foreground text-xs font-medium tracking-wide uppercase">Issue</p>
            <h2 id="drawer-title" class="text-lg">
              <IssueId identifier={props.identifier} url={props.issueUrl} />
            </h2>
            {detail.data !== undefined && !notFound && (
              <Badge tone={STATUS_TONE[detail.data.status]}>{detail.data.status}</Badge>
            )}
          </div>
          <Button onClick={props.onClose} aria-label="Close issue details">
            <Icon name="close" />
            Close
          </Button>
        </header>

        <div class="flex-1 space-y-4 overflow-y-auto px-5 py-4">
          {notFound ? (
            <Empty>{props.identifier} is not running, retrying or blocked right now.</Empty>
          ) : detail.error !== undefined ? (
            <ErrorText>Could not load issue: {describeError(detail.error)}</ErrorText>
          ) : detail.data === undefined ? (
            <p class="text-muted-foreground">Loading…</p>
          ) : (
            <IssueBody detail={detail.data} now={props.now} />
          )}

          {props.storeEnabled && runs.data != null && (
            <RecentRuns runs={runs.data} identifier={props.identifier} />
          )}

          <p class="text-xs">
            <a href={`/api/v1/${encodeURIComponent(props.identifier)}`}>Raw JSON</a>
          </p>
        </div>
      </div>
    </dialog>
  );
}
