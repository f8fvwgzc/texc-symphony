/** Joins class names, skipping `false`, `null` and `undefined`. */
export function cx(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(' ');
}
