import type { EventSourceLike } from '../live/liveSource';

type Listener = (event: MessageEvent<string>) => void;

/** In-memory `EventSource` double; every instance is recorded in `FakeEventSource.instances`. */
export class FakeEventSource implements EventSourceLike {
  static instances: FakeEventSource[] = [];

  static reset(): void {
    FakeEventSource.instances = [];
  }

  static latest(): FakeEventSource {
    const last = FakeEventSource.instances[FakeEventSource.instances.length - 1];
    if (last === undefined) throw new Error('no EventSource was created');
    return last;
  }

  static factory = (url: string): FakeEventSource => new FakeEventSource(url);

  readonly url: string;
  closed = false;
  private readonly listeners = new Map<string, Listener[]>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: Listener): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  close(): void {
    this.closed = true;
  }

  emit(type: string, data = '', lastEventId = ''): void {
    const event = new MessageEvent<string>(type, { data, lastEventId });
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }

  open(): void {
    this.emit('open');
  }

  fail(): void {
    this.emit('error');
  }

  snapshot(payload: unknown, generation: number): void {
    this.emit('snapshot', JSON.stringify(payload), String(generation));
  }

  heartbeat(generation = 0): void {
    this.emit('heartbeat', JSON.stringify({ at: '2026-02-24T20:15:45Z', generation }));
  }
}
