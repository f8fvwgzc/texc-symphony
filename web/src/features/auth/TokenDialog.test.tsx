import { act, fireEvent, screen, within } from '@testing-library/preact';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { App } from '../../App';
import { ApiError, type ApiClient } from '../../api/client';
import { createTokenStore, memoryStorage, TOKEN_STORAGE_KEY } from '../../api/token';
import { makeFakeApi } from '../../test/fakeApi';
import { FakeEventSource } from '../../test/fakeEventSource';
import { makeSnapshot } from '../../test/fixtures';
import { renderWithApi } from '../../test/render';

const TICKET = { expires_at: '2026-02-24T20:16:30Z', expires_in_ms: 60_000 };

/** `createStreamTicket` refusing the first `refusals` calls with 401. */
function ticketsRefused(refusals: number) {
  let calls = 0;
  return vi.fn<ApiClient['createStreamTicket']>(() => {
    calls += 1;
    return calls <= refusals ? Promise.reject(unauthorized()) : Promise.resolve(TICKET);
  });
}

const unauthorized = () => new ApiError(401, 'unauthorized', 'Missing or invalid API token');

function renderApp(api: ReturnType<typeof makeFakeApi>) {
  return renderWithApi(
    <App liveOptions={{ createEventSource: FakeEventSource.factory, random: () => 0.5 }} />,
    api,
  );
}

async function enterToken(token: string, remember = false) {
  const dialog = await screen.findByRole('dialog', { name: 'API token required' });
  fireEvent.input(within(dialog).getByLabelText('API token'), { target: { value: token } });
  if (remember) fireEvent.click(within(dialog).getByLabelText('Remember on this device'));
  await act(async () => {
    fireEvent.click(within(dialog).getByRole('button', { name: 'Connect' }));
  });
  return dialog;
}

beforeEach(() => {
  FakeEventSource.reset();
  window.location.hash = '';
});

describe('API token dialog', () => {
  it('appears when the stream ticket is refused and reconnects with the new token', async () => {
    const createStreamTicket = ticketsRefused(1);
    const api = makeFakeApi({ createStreamTicket });
    renderApp(api);
    expect(await screen.findByRole('dialog', { name: 'API token required' })).toBeInTheDocument();
    expect(screen.getAllByRole('status')[0]).toHaveTextContent('Token needed');
    expect(FakeEventSource.instances).toHaveLength(0);

    await enterToken('s3cret-token');
    expect(api.getState).toHaveBeenCalledTimes(1);
    expect(api.auth.get()).toBe('s3cret-token');
    expect(screen.queryByRole('dialog', { name: 'API token required' })).toBeNull();

    const es = await FakeEventSource.opened();
    expect(createStreamTicket).toHaveBeenCalledTimes(2);
    expect(es.url).toBe('/api/v1/events');
    await act(() => es.snapshot(makeSnapshot(), 2));
    expect(screen.getAllByRole('status')[0]).toHaveTextContent('Live');
    for (const instance of FakeEventSource.instances) {
      expect(instance.url).not.toContain('s3cret-token');
    }
  });

  it('shows an error and forgets a token the server refuses', async () => {
    const api = makeFakeApi({
      createStreamTicket: ticketsRefused(Number.POSITIVE_INFINITY),
      getState: vi.fn<ApiClient['getState']>(() => Promise.reject(unauthorized())),
    });
    renderApp(api);
    const dialog = await enterToken('wrong');
    expect(await within(dialog).findByRole('alert')).toHaveTextContent(
      'The server did not accept this token.',
    );
    expect(api.auth.get()).toBeNull();
    expect(screen.getByRole('dialog', { name: 'API token required' })).toBeInTheDocument();
  });

  it('asks up front when the server requires a token and none is stored', async () => {
    const api = makeFakeApi({
      getHealth: vi.fn<ApiClient['getHealth']>(() =>
        Promise.resolve({
          status: 'ok' as const,
          version: '0.1.0',
          uptime_seconds: 1,
          store: 'sqlite' as const,
          auth_required: true,
        }),
      ),
    });
    renderApp(api);
    expect(await screen.findByRole('dialog', { name: 'API token required' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /Forget the API token/ })).toBeNull();
  });

  it('remembers the token on this device when asked, and forgets it from the header', async () => {
    const session = memoryStorage();
    const local = memoryStorage();
    const createStreamTicket = ticketsRefused(1);
    const api = makeFakeApi({ auth: createTokenStore(session, local), createStreamTicket });
    renderApp(api);
    await enterToken('keep-me', true);
    expect(local.items.get(TOKEN_STORAGE_KEY)).toBe('keep-me');
    expect(session.items.has(TOKEN_STORAGE_KEY)).toBe(false);

    await FakeEventSource.opened();
    await act(() => {
      fireEvent.click(screen.getByRole('button', { name: 'Forget the API token on this device' }));
    });
    expect(api.auth.get()).toBeNull();
    expect(local.items.size).toBe(0);
    expect(session.items.size).toBe(0);
    expect(screen.queryByRole('button', { name: /Forget the API token/ })).toBeNull();
    // The live view starts over (a fresh ticket request, without the forgotten token).
    await FakeEventSource.opened(2);
    expect(createStreamTicket).toHaveBeenCalledTimes(3);
  });
});
