// The parts of a date, time or timestamp cell as the grid shows it, for the date and time
// picker (components/ValuePicker.tsx). Tested by scripts/time-text.test.mjs.

export interface TimeParts {
  date: string; // yyyy-mm-dd, as the date input writes it; "" when not set
  time: string; // hh:mm or hh:mm:ss, as the time input writes it; "" when not set
  fraction: string;
  offset: string; // +hh:mm
}

/**
 * The parts of a date, time or timestamp as the grid writes it: `2026-01-31 10:20:30.5+00`,
 * or SQL Server's `datetimeoffset` with every digit and its own offset after a space
 * (`2026-01-31 10:20:30.1234567 +02:00`).
 */
export function parseTimeText(text: string | null): TimeParts {
  const parts: TimeParts = { date: "", time: "", fraction: "", offset: "+00:00" };
  if (!text) return parts;
  const date = /^(\d{4}-\d{2}-\d{2})/.exec(text);
  if (date) parts.date = date[1]!;
  const time = /(?:^|[ T])(\d{2}:\d{2}:\d{2})(?:\.(\d+))?/.exec(text);
  if (time) {
    parts.time = time[1]!;
    parts.fraction = time[2] ?? "";
  }
  const offset = /\d ?([+-])(\d{2})(?::?(\d{2}))?$/.exec(text);
  if (offset && parts.time) parts.offset = `${offset[1]}${offset[2]}:${offset[3] ?? "00"}`;
  return parts;
}

/** `+05:30` → 330; `null` when it is not an offset. */
export function offsetMinutes(text: string): number | null {
  const m = /^([+-])(\d{1,2})(?::?(\d{2}))?$/.exec(text.trim());
  if (!m) return null;
  const minutes = Number(m[2]) * 60 + Number(m[3] ?? 0);
  return m[1] === "-" ? -minutes : minutes;
}
