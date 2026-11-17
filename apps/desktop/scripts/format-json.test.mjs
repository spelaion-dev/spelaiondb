import assert from "node:assert/strict";
import test from "node:test";
import { formatJson } from "../src/formatJson.ts";

test("formatting preserves numeric lexemes, duplicate keys and escaped strings", () => {
  const text = '{"n":12345678901234567890,"n":1.0,"exp":1e+30,"s":"a\\\"b", "a":[{},[]]}';
  const formatted = formatJson(text);
  assert.equal(formatted, '{\n  "n": 12345678901234567890,\n  "n": 1.0,\n  "exp": 1e+30,\n  "s": "a\\\"b",\n  "a": [\n    {},\n    []\n  ]\n}');
  assert.deepEqual(JSON.parse(formatted), JSON.parse(text));
});
test("invalid JSON stays untouched and cannot be formatted", () => {
  for (const text of ['{"x":}', '[1,]', '{"unfinished"', 'NaN', '']) assert.equal(formatJson(text), null);
});
test("already formatted values are idempotent", () => {
  for (const text of ['{}', '[]', 'null', '[1, {"a":"x"}]']) {
    const once = formatJson(text); assert.notEqual(once, null); assert.equal(formatJson(once), once);
  }
});
