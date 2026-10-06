import { render, screen, within } from '@testing-library/preact';
import { describe, expect, it } from 'vitest';

import { at } from '../../test/dom';
import { makeEmptySnapshot, makeSnapshot, SNAPSHOT_TIMEOUT, TOTALS } from '../../test/fixtures';
import { OverviewPage } from './OverviewPage';

const NOW = Date.parse('2026-02-24T20:15:30Z');

describe('OverviewPage', () => {
  it('shows a waiting message before the first snapshot', () => {
    render(<OverviewPage state={null} totals={null} now={NOW} />);
    expect(screen.getByText('Waiting for the first snapshot…')).toBeInTheDocument();
  });

  it('renders the snapshot error card', () => {
    render(<OverviewPage state={SNAPSHOT_TIMEOUT} totals={null} now={NOW} />);
    const alert = screen.getByRole('alert');
    expect(alert).toHaveTextContent('Snapshot unavailable');
    expect(alert).toHaveTextContent('snapshot_timeout: Snapshot timed out');
    expect(screen.queryByText('Running sessions')).not.toBeInTheDocument();
  });

  it('renders metrics with live runtime and all-time totals', () => {
    render(<OverviewPage state={makeSnapshot()} totals={TOTALS} now={NOW} />);
    const summary = screen.getByRole('region', { name: 'Summary' });
    expect(within(summary).getByText('Total tokens').nextElementSibling).toHaveTextContent('12');
    expect(within(summary).getByText('In 4 / Out 8')).toBeInTheDocument();
    // 42 s ended + 318 s live elapsed.
    expect(within(summary).getByText('6m 0s')).toBeInTheDocument();
    expect(within(summary).getByText('All-time runs').nextElementSibling).toHaveTextContent('128');
    expect(
      within(summary).getByText(/76% succeeded · 21 failed · 2.1M tokens/),
    ).toBeInTheDocument();
  });

  it('renders running, blocked and retry rows', () => {
    render(<OverviewPage state={makeSnapshot()} totals={null} now={NOW} />);

    const running = screen.getByRole('table', { name: 'Running sessions' });
    const cells = within(at(within(running).getAllByRole('row'), 1));
    expect(cells.getByRole('link', { name: 'Open MT-HTTP in the issue tracker' })).toHaveAttribute(
      'href',
      'https://example.org/issues/MT-HTTP',
    );
    expect(cells.getByRole('link', { name: 'Details for MT-HTTP' })).toHaveAttribute(
      'href',
      '#/issues/MT-HTTP',
    );
    expect(cells.getByRole('link', { name: 'JSON for MT-HTTP' })).toHaveAttribute(
      'href',
      '/api/v1/MT-HTTP',
    );
    expect(cells.getByText('In Progress')).toHaveAttribute('data-tone', 'active');
    expect(cells.getByText('5m 18s / 7')).toBeInTheDocument();
    expect(cells.getByText('rendered')).toBeInTheDocument();
    expect(cells.getByRole('button', { name: 'Copy session ID thread-http' })).toBeInTheDocument();

    const blocked = screen.getByRole('table', { name: 'Blocked sessions' });
    expect(within(blocked).getByText('codex turn requires operator input')).toBeInTheDocument();
    expect(within(blocked).getByText('turn blocked: waiting for user input')).toBeInTheDocument();
    expect(within(blocked).getAllByText('1m ago')).toHaveLength(2);

    const retry = screen.getByRole('table', { name: 'Retry queue' });
    expect(within(retry).getByText('boom')).toBeInTheDocument();
    expect(within(retry).getByText('in 2s')).toBeInTheDocument();
  });

  it('shows empty states', () => {
    render(<OverviewPage state={makeEmptySnapshot()} totals={null} now={NOW} />);
    expect(screen.getByText('No active sessions.')).toBeInTheDocument();
    expect(screen.getByText('No blocked sessions.')).toBeInTheDocument();
    expect(screen.getByText('No issues are currently backing off.')).toBeInTheDocument();
    expect(screen.getByText('No rate-limit data reported yet.')).toBeInTheDocument();
    expect(screen.queryByText('All-time runs')).not.toBeInTheDocument();
  });

  it('does not link unsafe tracker URLs', () => {
    const snapshot = makeSnapshot();
    const [entry] = snapshot.running;
    if (entry === undefined) throw new Error('fixture');
    entry.issue_url = 'javascript:alert(1)';
    render(<OverviewPage state={snapshot} totals={null} now={NOW} />);
    expect(screen.queryByRole('link', { name: 'Open MT-HTTP in the issue tracker' })).toBeNull();
  });
});
