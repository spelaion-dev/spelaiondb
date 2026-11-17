// Check the final manifest after all platform jobs have uploaded, before publishing the draft.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export function checkManifest(manifest, version, notes) {
  const normalize = (value) => value.trim();
  if (!version || manifest.version !== version) throw new Error("Updater version does not match the release");
  if (typeof notes !== "string" || !normalize(notes) || typeof manifest.notes !== "string" || normalize(manifest.notes) !== normalize(notes)) {
    throw new Error("Updater notes are missing or differ from the release notes");
  }
  for (const platform of ["windows-x86_64", "darwin-aarch64", "darwin-x86_64", "linux-x86_64"]) {
    const entry = manifest.platforms?.[platform];
    if (typeof entry?.signature !== "string" || !entry.signature.trim() || typeof entry.url !== "string" || !entry.url.startsWith("https://")) {
      throw new Error(`Updater artifact or signature missing for ${platform}`);
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    checkManifest(JSON.parse(readFileSync(process.argv[2], "utf8")), process.argv[3], process.env.EXPECTED_NOTES);
    console.log("Updater manifest matches the release and includes every platform.");
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
