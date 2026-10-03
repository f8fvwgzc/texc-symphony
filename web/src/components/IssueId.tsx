import { externalIssueUrl } from '../lib/format';

/** Issue identifier, linked to the tracker when the URL is a safe http(s) URL. */
export function IssueId({ identifier, url }: { identifier: string; url: string | null }) {
  const href = externalIssueUrl(url);
  if (href === null) return <span class="issue-id">{identifier}</span>;
  return (
    <a
      class="issue-id issue-id-link"
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      aria-label={`Open ${identifier} in the issue tracker`}
    >
      {identifier}
    </a>
  );
}
