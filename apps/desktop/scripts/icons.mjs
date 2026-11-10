// Regenerates the app's icons from their SVG sources in src-tauri/icons: `npm run icons`
// (see "App icon" in docs/ui-guidelines.md).
//
// `tauri icon` draws every size from icon.svg. Scaled down to 16px and 24px the mark blurs, so
// those two images inside icon.ico are replaced with icon-16.svg and icon-24.svg, drawn on the
// pixel grid. Commit the result: nothing regenerates the icons when the app is built.
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { readIco, writeIco } from "./ico.mjs";

const desktop = resolve(import.meta.dirname, "..");
const icons = join(desktop, "src-tauri/icons");
const cliPackage = createRequire(import.meta.url).resolve("@tauri-apps/cli/package.json");
const cli = join(dirname(cliPackage), "tauri.js");
/** Sizes that have a source of their own, `icon-<size>.svg`. */
const HAND_DRAWN = [16, 24];

function tauriIcon(source, output, pngSizes) {
  const args = [cli, "icon", source, "--output", output];
  if (pngSizes) args.push("--png", pngSizes.join(","));
  execFileSync(process.execPath, args, { cwd: desktop, stdio: ["ignore", "ignore", "inherit"] });
}

const work = mkdtempSync(join(tmpdir(), "spelaion-icons-"));
try {
  const generated = join(work, "all");
  tauriIcon(join(icons, "icon.svg"), generated);
  // The files only: `tauri icon` also writes Android and iOS folders, and the app ships neither.
  for (const entry of readdirSync(generated, { withFileTypes: true })) {
    if (entry.isFile()) copyFileSync(join(generated, entry.name), join(icons, entry.name));
  }

  const images = readIco(readFileSync(join(generated, "icon.ico")));
  for (const size of HAND_DRAWN) {
    const drawn = join(work, String(size));
    tauriIcon(join(icons, `icon-${size}.svg`), drawn, [size]);
    const image = images.find((candidate) => candidate.size === size);
    if (!image) throw new Error(`icon.ico has no ${size}px image to replace`);
    image.png = readFileSync(join(drawn, `${size}x${size}.png`));
  }
  writeFileSync(join(icons, "icon.ico"), writeIco(images));
} finally {
  rmSync(work, { recursive: true, force: true });
}
console.log(`Icons regenerated in ${icons}`);
