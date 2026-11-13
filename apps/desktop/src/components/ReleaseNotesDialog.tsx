import { Fragment, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { manualUpdateKey, useUpdate } from "../update";

/** Render the small Markdown subset used in release notes as React text, never HTML.
 * Links are shown as text: release metadata cannot navigate the app or launch a URL. */
function inlineNotes(text: string): ReactNode[] {
  const tokens = text.split(/(\*\*[^*\n]+\*\*|`[^`\n]+`|\[[^\]\n]+\]\([^)\n]+\))/g);
  return tokens.map((token, index) => {
    if (token.startsWith("**") && token.endsWith("**")) return <strong key={index}>{token.slice(2, -2)}</strong>;
    if (token.startsWith("`") && token.endsWith("`")) return <code key={index}>{token.slice(1, -1)}</code>;
    const link = /^\[([^\]]+)\]\(([^)]+)\)$/.exec(token);
    if (link) return <Fragment key={index}>{link[1]} <span className="muted">({link[2]})</span></Fragment>;
    return <Fragment key={index}>{token}</Fragment>;
  });
}

type NotesBlock = { kind: "heading"; text: string } | { kind: "paragraph"; text: string } | { kind: "list"; items: string[] };

/** Markdown source wraps prose at arbitrary columns. Join continuation lines for the dialog. */
function notesBlocks(notes: string): NotesBlock[] {
  const blocks: NotesBlock[] = [];
  let paragraph = "";
  let items: string[] = [];
  const flushParagraph = () => {
    if (paragraph) blocks.push({ kind: "paragraph", text: paragraph });
    paragraph = "";
  };
  const flushList = () => {
    if (items.length) blocks.push({ kind: "list", items });
    items = [];
  };
  for (const sourceLine of notes.split(/\r?\n/)) {
    const line = sourceLine.trim();
    const heading = /^#{1,3}\s+(.+)$/.exec(line);
    const item = /^[-*]\s+(.+)$/.exec(line);
    if (!line) { flushParagraph(); flushList(); }
    else if (heading) { flushParagraph(); flushList(); blocks.push({ kind: "heading", text: heading[1] ?? "" }); }
    else if (item) { flushParagraph(); items.push(item[1] ?? ""); }
    else if (items.length) { items[items.length - 1] += ` ${line}`; }
    else { paragraph += `${paragraph ? " " : ""}${line}`; }
  }
  flushParagraph();
  flushList();
  return blocks;
}

export function ReleaseNotesDialog() {
  const { t } = useTranslation("app");
  const status = useUpdate((s) => s.status);
  const open = useUpdate((s) => s.notesOpen);
  const installing = useUpdate((s) => s.installing);
  const error = useUpdate((s) => s.error);
  const install = useUpdate((s) => s.install);
  const dismiss = useUpdate((s) => s.dismiss);
  const hide = useUpdate((s) => s.hideNotes);
  const openReleasesPage = useUpdate((s) => s.openReleasesPage);
  if (!open || status?.kind !== "available") return null;
  const manual = manualUpdateKey(status.method);
  const blocks = status.notes?.trim() ? notesBlocks(status.notes) : null;
  return createPortal(
    <div className="modal-backdrop" onKeyDown={(event) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        if (!installing) hide();
      }
    }}>
      <section className="modal release-notes-dialog" role="dialog" aria-modal="true" aria-labelledby="release-notes-title">
        <h2 id="release-notes-title">{t("update.notesTitle", { version: status.version })}</h2>
        <div className="release-notes-body" tabIndex={0}>
          {blocks ? blocks.map((block, index) => {
            if (block.kind === "heading") return <h3 key={index}>{inlineNotes(block.text)}</h3>;
            if (block.kind === "paragraph") return <p key={index}>{inlineNotes(block.text)}</p>;
            return <ul key={index}>{block.items.map((item, itemIndex) => <li key={itemIndex}>{inlineNotes(item)}</li>)}</ul>;
          }) : <p className="muted">{t("update.notesUnavailable")}</p>}
        </div>
        {manual && <p className="muted">{t(`update.${manual}`)}</p>}
        {error && <p className="error" role="alert">{error}</p>}
        <div className="modal-actions">
          <button disabled={installing} onClick={() => { dismiss(); hide(); }}>{t("update.later")}</button>
          {manual ? (
            <button className="primary" onClick={() => void openReleasesPage()}>{t("update.openReleases")}</button>
          ) : (
            <button className="primary" disabled={installing} onClick={() => void install()}>
              {installing ? t("update.installing") : t("update.install")}
            </button>
          )}
        </div>
      </section>
    </div>,
    document.body,
  );
}
