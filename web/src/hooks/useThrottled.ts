import { useEffect, useRef, useState } from 'preact/hooks';

/** Follows `value`, but changes at most once per `intervalMs` (trailing edge is kept). */
export function useThrottled<T>(value: T, intervalMs: number): T {
  const [throttled, setThrottled] = useState(value);
  const lastEmit = useRef(0);

  useEffect(() => {
    const wait = lastEmit.current + intervalMs - Date.now();
    if (wait <= 0) {
      lastEmit.current = Date.now();
      setThrottled(value);
      return undefined;
    }
    const timer = setTimeout(() => {
      lastEmit.current = Date.now();
      setThrottled(value);
    }, wait);
    return () => clearTimeout(timer);
  }, [value, intervalMs]);

  return throttled;
}
