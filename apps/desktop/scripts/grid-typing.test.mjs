import assert from "node:assert/strict";
import { test } from "node:test";
import { printableGridKey } from "../src/gridTyping.ts";

const key = (key, extra = {}) => ({ key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, isComposing: false, ...extra });

test("printable characters including shifted and non-ASCII characters start editing", () => {
  for (const event of [key("a"), key("A", { shiftKey: true }), key("ñ"), key(" ")]) assert.equal(printableGridKey(event), true);
});
test("shortcuts, navigation, row selection and composition do not start editing", () => {
  for (const event of [key("f", { ctrlKey: true }), key("a", { metaKey: true }), key("x", { altKey: true }), key(" ", { shiftKey: true }), key(" ", { ctrlKey: true }), key("Enter"), key("Delete"), key("ArrowLeft"), key("a", { isComposing: true }), key("Dead")]) assert.equal(printableGridKey(event), false);
});
