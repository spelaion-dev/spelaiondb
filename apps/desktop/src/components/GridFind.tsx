// The result grid's find bar (Ctrl+F). The search runs in the core over every loaded row;
// this only edits the text and shows "N of M".

import type { RefObject } from "react";
import { useTranslation } from "react-i18next";
import { keyText } from "../shortcuts";
import { Icon } from "./Icons";

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
  const previous = t("results.findPrevious", { inBox: keyText("findPreviousInBox"), shortcut: keyText("findPrevious") });
  const next = t("results.findNext", { inBox: keyText("findNextInBox"), shortcut: keyText("findNext") });
  const close = t("results.findClose", { shortcut: keyText("close") });
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
        aria-label={t("results.findMatchCase", { shortcut: keyText("findMatchCase") })}
        title={t("results.findMatchCase", { shortcut: keyText("findMatchCase") })}
        onClick={() => props.onMatchCase(!props.matchCase)}
      >
        Aa
      </button>
      <span className="grid-find-status small muted" role="status">
        {props.status}
      </span>
      <button type="button" className="ghost small" aria-label={previous} title={previous} disabled={!props.canStep} onClick={props.onPrevious}>
        <Icon name="arrowUp" />
      </button>
      <button type="button" className="ghost small" aria-label={next} title={next} disabled={!props.canStep} onClick={props.onNext}>
        <Icon name="arrowDown" />
      </button>
      <button type="button" className="ghost small" aria-label={close} title={close} onClick={props.onClose}>
        <Icon name="x" />
      </button>
    </div>
  );
}
