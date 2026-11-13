// The date and time picker's reading of a cell (src/timeText.ts), run by `npm run build` with
// `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { offsetMinutes, parseTimeText } from "../src/timeText.ts";

test("a time zone shown in UTC", () => {
  assert.deepEqual(parseTimeText("2026-01-31 10:20:30.5+00"), {
    date: "2026-01-31",
    time: "10:20:30",
    fraction: "5",
    offset: "+00:00",
  });
});

test("SQL Server's datetimeoffset: every digit and its own offset after a space", () => {
  assert.deepEqual(parseTimeText("2026-01-31 10:20:30.1234567 +02:00"), {
    date: "2026-01-31",
    time: "10:20:30",
    fraction: "1234567",
    offset: "+02:00",
  });
  assert.equal(parseTimeText("2026-01-31 10:20:30 -05:30").offset, "-05:30");
});

test("dates, times and timestamps without a zone keep the default offset", () => {
  assert.deepEqual(parseTimeText("2026-01-31"), {
    date: "2026-01-31",
    time: "",
    fraction: "",
    offset: "+00:00",
  });
  assert.deepEqual(parseTimeText("2026-01-31 10:20:30.1234567"), {
    date: "2026-01-31",
    time: "10:20:30",
    fraction: "1234567",
    offset: "+00:00",
  });
  assert.equal(parseTimeText("10:20:30").offset, "+00:00");
  assert.equal(parseTimeText(null).date, "");
});

test("offsets in minutes", () => {
  assert.equal(offsetMinutes("+05:30"), 330);
  assert.equal(offsetMinutes("-02:00"), -120);
  assert.equal(offsetMinutes("2:00"), null);
});
