// The result grid's find bar (Ctrl+F). The search runs in the core over every loaded row;
// this only edits the text and shows "N of M".

import type { RefObject } from "react";
import { useTranslation } from "react-i18next";

export function FindBar(props: {
  inputRef: RefObject<HTMLInputElement | null>;
  text: string;
  matchCase: boolean;
  status: string;
  canStep: boolean;
  onText(text: string): void;
  onMatchCase(matchCase: boolean): void;
  onNext(): void;
  onPrevious(): void;
  onClose(): void;
}) {
  const { t } = useTranslation("editor");
  return (
    <div
      className="grid-find"
      role="search"
      aria-label={t("results.find")}
      onKeyDown={(e) => {
        if (e.key === "Escape") props.onClose();
        else if (e.key === "Enter" || e.key === "F3") {
          if (props.canStep) {
            if (e.shiftKey) props.onPrevious();
            else props.onNext();
          }
        } else if (e.altKey && e.key.toLowerCase() === "c") props.onMatchCase(!props.matchCase);
        else return;
        e.preventDefault();
        e.stopPropagation();
      }}
    >
      <input
        ref={props.inputRef}
        type="search"
        value={props.text}
        placeholder={t("results.findPlaceholder")}
        aria-label={t("results.find")}
        spellCheck={false}
        onChange={(e) => props.onText(e.target.value)}
      />
      <button
        type="button"
        className={`ghost small grid-find-case ${props.matchCase ? "active" : ""}`}
        aria-pressed={props.matchCase}
        aria-label={t("results.findMatchCase")}
        title={t("results.findMatchCase")}
        onClick={() => props.onMatchCase(!props.matchCase)}
      >
        Aa
      </button>
      <span className="grid-find-status small muted" role="status">
        {props.status}
      </span>
      <button type="button" className="ghost small" aria-label={t("results.findPrevious")} title={t("results.findPrevious")} disabled={!props.canStep} onClick={props.onPrevious}>
        ↑
      </button>
      <button type="button" className="ghost small" aria-label={t("results.findNext")} title={t("results.findNext")} disabled={!props.canStep} onClick={props.onNext}>
        ↓
      </button>
      <button type="button" className="ghost small" aria-label={t("results.findClose")} title={t("results.findClose")} onClick={props.onClose}>
        ×
      </button>
    </div>
  );
}
