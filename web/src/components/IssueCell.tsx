import { formatRoute } from '../lib/router';
import { IssueId } from './IssueId';

/** Issue column: tracker link + drawer link + raw JSON link. */
export function IssueCell({ identifier, url }: { identifier: string; url: string | null }) {
  return (
    <div class="stack">
      <IssueId identifier={identifier} url={url} />
      <span class="issue-links">
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
