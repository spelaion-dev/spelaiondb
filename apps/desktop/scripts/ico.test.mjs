// The .ico reader and writer of `npm run icons` (scripts/ico.mjs) and the icon file the app
// ships, run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import { pngSize, readIco, writeIco } from "./ico.mjs";

/** The first bytes of a PNG of that size: its signature and the start of its header chunk. */
function png(width, height = width, filler = 0) {
  const bytes = Buffer.alloc(24 + filler, 0xab);
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).copy(bytes);
  bytes.writeUInt32BE(13, 8);
  bytes.write("IHDR", 12, "latin1");
  bytes.writeUInt32BE(width, 16);
  bytes.writeUInt32BE(height, 20);
  return bytes;
}

test("a PNG's size is read from its header", () => {
  assert.deepEqual(pngSize(png(24)), { width: 24, height: 24 });
  assert.deepEqual(pngSize(png(300, 2)), { width: 300, height: 2 });
  assert.throws(() => pngSize(Buffer.from("<svg/>")), /Not a PNG/);
  assert.throws(() => pngSize(Buffer.alloc(40)), /Not a PNG/);
});

test("what is written is read back, image by image and in order", () => {
  const images = [
    { size: 16, png: png(16, 16, 5) },
    { size: 256, png: png(256, 256, 900) },
    { size: 24, png: png(24, 24, 0) },
  ];
  const read = readIco(writeIco(images));
  assert.deepEqual(read.map((image) => image.size), [16, 256, 24]);
  read.forEach((image, i) => assert.ok(image.png.equals(images[i].png)));
});

test("256px is written as 0 in the directory's one-byte size", () => {
  const ico = writeIco([{ size: 256, png: png(256) }, { size: 48, png: png(48) }]);
  assert.equal(ico[6], 0);
  assert.equal(ico[7], 0);
  assert.equal(ico[6 + 16], 48);
});

test("an image that is not what its entry says is refused", () => {
  assert.throws(() => writeIco([{ size: 16, png: png(24) }]), /16px image is 24px/);
  assert.throws(() => writeIco([{ size: 16, png: png(16, 24) }]), /must be square/);
  assert.throws(() => writeIco([{ size: 512, png: png(512) }]), /at most 256px/);
  assert.throws(() => writeIco([{ size: 16, png: Buffer.from("not an image at all, just text") }]), /Not a PNG/);
});

test("a file that is not an .ico, or is cut short, is refused", () => {
  assert.throws(() => readIco(Buffer.from("PNG")), /Not an \.ico/);
  assert.throws(() => readIco(png(16)), /Not an \.ico/);
  const ico = writeIco([{ size: 16, png: png(16, 16, 50) }, { size: 32, png: png(32, 32, 50) }]);
  assert.throws(() => readIco(ico.subarray(0, 30)), /Truncated \.ico directory/);
  assert.throws(() => readIco(ico.subarray(0, ico.length - 1)), /Truncated \.ico image/);
});

test("the app's icon.ico holds every size Windows asks for, and writes back byte for byte", () => {
  const bytes = readFileSync(resolve(import.meta.dirname, "../src-tauri/icons/icon.ico"));
  const images = readIco(bytes);
  assert.deepEqual(images.map((image) => image.size), [16, 24, 32, 48, 64, 256]);
  assert.ok(writeIco(images).equals(bytes));
});
