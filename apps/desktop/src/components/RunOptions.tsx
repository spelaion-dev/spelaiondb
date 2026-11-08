// A tab's run options (ADR-035): what its runs do when a statement fails and their statement
// timeout. They only choose; the core runs the statements, decides what a failure allows and
// refuses unconfirmed risky scripts whatever the mode.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { OnErrorItem, RunOptionsItem } from "../api";
import { usePreferences } from "../preferences";
import { useApp, type Tab } from "../store";

/** What a run of `tab` sends: its own choices, or the personal default and the connection's
 *  timeout. */
export function runOptionsOf(tab: Tab): RunOptionsItem {
  return {
    onError: tab.runOptions?.onError ?? usePreferences.getState().script.onError,
    timeoutSeconds: tab.runOptions?.timeoutSeconds ?? null,
  };
}

const MODES: OnErrorItem[] = ["stop", "continue", "ask"];

export function RunOptionsButton({ tab, disabled }: { tab: Tab; disabled: boolean }) {
  const { t } = useTranslation("editor");
  const updateTab = useApp((s) => s.updateTab);
  const defaultMode = usePreferences((s) => s.script.onError);
  const [open, setOpen] = useState<{ top: number; left: number } | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const own = tab.runOptions;
  const mode = own?.onError ?? defaultMode;
  const [timeout, setTimeoutText] = useState(own?.timeoutSeconds == null ? "" : String(own.timeoutSeconds));
  useEffect(() => setTimeoutText(own?.timeoutSeconds == null ? "" : String(own.timeoutSeconds)), [own?.timeoutSeconds]);

  useEffect(() => {
    if (!open) return;
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && root.current?.contains(e.target as Node)) return;
      setOpen(null);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [open]);

  const set = (next: { onError?: OnErrorItem | null; timeoutSeconds?: number | null }) => {
    const merged = {
      onError: next.onError !== undefined ? next.onError : own?.onError ?? null,
      timeoutSeconds: next.timeoutSeconds !== undefined ? next.timeoutSeconds : own?.timeoutSeconds ?? null,
    };
    updateTab(tab.id, { runOptions: merged.onError == null && merged.timeoutSeconds == null ? undefined : merged });
  };
  const saveTimeout = () => {
    if (timeout.trim() === "") return set({ timeoutSeconds: null });
    const seconds = Math.trunc(Number(timeout));
    if (Number.isFinite(seconds) && seconds >= 0 && seconds <= 86400) set({ timeoutSeconds: seconds });
    else setTimeoutText(own?.timeoutSeconds == null ? "" : String(own.timeoutSeconds));
  };
  const changed = own !== undefined;

  return (
    <div className="run-options" ref={root}>
      <button
        className={`editor-action${changed ? " editor-action-on" : ""}`}
        aria-label={t("runOptions.button")}
        aria-haspopup="dialog"
        aria-expanded={open !== null}
        title={`${t("runOptions.button")}: ${t(`runOptions.mode.${mode}`)}${own?.timeoutSeconds ? ` · ${own.timeoutSeconds} s` : ""}`}
        disabled={disabled}
        onClick={(e) => {
          const rect = e.currentTarget.getBoundingClientRect();
          setOpen(open ? null : { top: rect.top, left: rect.right + 4 });
        }}
      >
        <svg viewBox="0 0 20 20" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
          <path d="M4 5h12M4 10h12M4 15h12" strokeLinecap="round" />
          <circle cx="8" cy="5" r="1.8" fill="var(--bg)" />
          <circle cx="13" cy="10" r="1.8" fill="var(--bg)" />
          <circle cx="7" cy="15" r="1.8" fill="var(--bg)" />
        </svg>
      </button>
      {open && (
        <div className="grid-menu run-options-panel" role="dialog" aria-label={t("runOptions.title")} style={open}>
          <strong className="small">{t("runOptions.title")}</strong>
          <fieldset>
            <legend className="small">{t("runOptions.onError")}</legend>
            <label className="check">
              <input type="radio" name={`onError-${tab.id}`} value="" checked={own?.onError == null} onChange={() => set({ onError: null })} />
              {t("runOptions.default", { mode: t(`runOptions.mode.${defaultMode}`) })}
            </label>
            {MODES.map((m) => (
              <label key={m} className="check">
                <input type="radio" name={`onError-${tab.id}`} value={m} checked={own?.onError === m} onChange={() => set({ onError: m })} />
                {t(`runOptions.mode.${m}`)}
              </label>
            ))}
          </fieldset>
          <label className="field">
            <span className="small">{t("runOptions.timeout")}</span>
            <input
              type="number"
              name="runTimeout"
              min={0}
              max={86400}
              value={timeout}
              placeholder={t("runOptions.timeoutConnection")}
              onChange={(e) => setTimeoutText(e.target.value)}
              onBlur={saveTimeout}
              onKeyDown={(e) => e.key === "Enter" && saveTimeout()}
            />
          </label>
          <p className="muted small">{t("runOptions.timeoutHint")}</p>
          {changed && (
            <button className="ghost small" onClick={() => { updateTab(tab.id, { runOptions: undefined }); setTimeoutText(""); }}>
              {t("runOptions.reset")}
            </button>
          )}
        </div>
      )}
    </div>
  );
}
