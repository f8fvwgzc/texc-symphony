/** `list[index]`, failing the test loudly instead of returning `undefined`. */
export function at<T>(list: readonly T[], index: number): T {
  const item = list[index];
  if (item === undefined) throw new Error(`no item at index ${index} (length ${list.length})`);
  return item;
}
