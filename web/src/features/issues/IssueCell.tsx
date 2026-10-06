import { formatRoute } from '../../lib/router';
import { IssueId } from './IssueId';

/** Issue column: tracker link, drawer link and raw JSON link. */
export function IssueCell({ identifier, url }: { identifier: string; url: string | null }) {
  return (
    <div class="flex flex-col gap-0.5">
      <IssueId identifier={identifier} url={url} />
      <span class="flex gap-3 text-xs">
        <a
          href={formatRoute({ name: 'overview', issue: identifier })}
          aria-label={`Details for ${identifier}`}
        >
          Details
        </a>
        <a href={`/api/v1/${encodeURIComponent(identifier)}`} aria-label={`JSON for ${identifier}`}>
          JSON
        </a>
      </span>
    </div>
  );
}
