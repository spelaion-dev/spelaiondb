// The .ico container the way `tauri icon` writes it: a directory of square PNG images, one per
// size. `icons.mjs` uses it to swap in the images drawn by hand for the small sizes.

const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
const DIRECTORY = 6;
const ENTRY = 16;
const LARGEST = 256;

/** The width and height a PNG file declares. Throws when the bytes are not a PNG. */
export function pngSize(png) {
  if (png.length < 24 || !png.subarray(0, 8).equals(PNG_SIGNATURE)) throw new Error("Not a PNG image");
  return { width: png.readUInt32BE(16), height: png.readUInt32BE(20) };
}

function squareSize(png) {
  const { width, height } = pngSize(png);
  if (width !== height || width < 1 || width > LARGEST) {
    throw new Error(`An .ico image must be square and at most ${LARGEST}px: ${width}x${height}`);
  }
  return width;
}

/** Reads an .ico file into `{ size, png }` images, in the file's order. */
export function readIco(bytes) {
  if (bytes.length < DIRECTORY || bytes.readUInt16LE(0) !== 0 || bytes.readUInt16LE(2) !== 1) {
    throw new Error("Not an .ico file");
  }
  const count = bytes.readUInt16LE(4);
  if (DIRECTORY + ENTRY * count > bytes.length) throw new Error("Truncated .ico directory");
  const images = [];
  for (let i = 0; i < count; i++) {
    const entry = DIRECTORY + ENTRY * i;
    const length = bytes.readUInt32LE(entry + 8);
    const offset = bytes.readUInt32LE(entry + 12);
    if (offset + length > bytes.length) throw new Error("Truncated .ico image");
    const png = bytes.subarray(offset, offset + length);
    images.push({ size: squareSize(png), png });
  }
  return images;
}

/** Writes `{ size, png }` images as an .ico file, in the given order. */
export function writeIco(images) {
  const directory = Buffer.alloc(DIRECTORY + ENTRY * images.length);
  directory.writeUInt16LE(1, 2);
  directory.writeUInt16LE(images.length, 4);
  let offset = directory.length;
  images.forEach(({ size, png }, i) => {
    if (squareSize(png) !== size) throw new Error(`The ${size}px image is ${squareSize(png)}px`);
    const entry = DIRECTORY + ENTRY * i;
    // One byte each for width and height: 0 stands for 256.
    directory[entry] = size % LARGEST;
    directory[entry + 1] = size % LARGEST;
    // Colour planes stay 0 and bits per pixel are 32, as `tauri icon` writes them: an untouched
    // file written back is the same file, byte for byte.
    directory.writeUInt16LE(32, entry + 6);
    directory.writeUInt32LE(png.length, entry + 8);
    directory.writeUInt32LE(offset, entry + 12);
    offset += png.length;
  });
  return Buffer.concat([directory, ...images.map((image) => image.png)]);
}
