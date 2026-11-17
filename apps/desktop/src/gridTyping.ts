/** Whether a grid key starts replacement editing instead of a shortcut or IME composition. */
export function printableGridKey(event: { key: string; ctrlKey: boolean; metaKey: boolean; altKey: boolean; shiftKey: boolean; isComposing: boolean }): boolean {
  return !event.ctrlKey && !event.metaKey && !event.altKey && !event.isComposing
    && Array.from(event.key).length === 1 && !(event.key === " " && event.shiftKey);
}
