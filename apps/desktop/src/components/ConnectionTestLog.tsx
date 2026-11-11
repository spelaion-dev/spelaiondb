// The connection test's log (ADR-032): one line per step as the core reports it, with Stop
// while it runs and "Copy diagnostics" when it ends. The core decides every step, failure
// kind and hint, and builds the diagnostics text (already redacted): this only shows them.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Channel } from "@tauri-apps/api/core";
import { commands, type TestReportItem, type TestStepItem } from "../api";

let nextTest = Math.floor(Math.random() * 1_000_000);

/** A test running or finished, and how to start and stop one. */
export function useConnectionTest() {
  const [steps, setSteps] = useState<TestStepItem[] | null>(null);
  const [report, setReport] = useState<TestReportItem | null>(null);
  const [running, setRunning] = useState(false);
  const current = useRef<number | null>(null);

  // A form closed while a test runs stops it: what it opened is closed in the core.
  useEffect(
    () => () => {
      if (current.current !== null) void commands.stopConnectionTest(current.current).catch(() => undefined);
    },
    [],
  );

  /** Runs `start` with a fresh test id and a channel for its steps. */
  async function run<T>(start: (test: number, onStep: Channel<TestStepItem[]>) => Promise<T>): Promise<T> {
    const test = ++nextTest;
    current.current = test;
    const channel = new Channel<TestStepItem[]>();
    channel.onmessage = (next) => {
      if (current.current === test) setSteps(next);
    };
    setSteps([]);
    setReport(null);
    setRunning(true);
    try {
      return await start(test, channel);
    } finally {
      if (current.current === test) {
        current.current = null;
        setRunning(false);
      }
    }
  }

  function finish(finished: TestReportItem) {
    setSteps(finished.steps);
    setReport(finished);
  }

  function stop() {
    if (current.current !== null) void commands.stopConnectionTest(current.current).catch(() => undefined);
  }

  /** Forgets the last test (the form changed). A running one is stopped. */
  function clear() {
    stop();
    current.current = null;
    setRunning(false);
    setSteps(null);
    setReport(null);
  }

  return { steps, report, running, run, finish, stop, clear };
}

const ICONS: Record<TestStepItem["status"], string> = {
  pending: "○",
  running: "…",
  ok: "✓",
  failed: "✗",
  skipped: "–",
  stopped: "■",
};

/** The step's line, translated from what the core says it is. */
function useStepTitle() {
  const { t } = useTranslation("connections");
  return (step: TestStepItem): string => {
    const kind = step.kind;
    switch (kind.kind) {
      case "resolve":
        return t("testLog.steps.resolve");
      case "proxy":
        return t("testLog.steps.proxy", { type: kind.proxyType.toUpperCase(), host: kind.host, port: kind.port });
      case "hop":
        return t(kind.command ? "testLog.steps.hopCommand" : "testLog.steps.hop", { hop: kind.hop, user: kind.user, host: kind.host, port: kind.port });
      case "kubernetes":
        return t("testLog.steps.kubernetes", { resource: kind.resource });
      case "ssm":
        return kind.region ? t("testLog.steps.ssmRegion", { target: kind.target, region: kind.region }) : t("testLog.steps.ssm", { target: kind.target });
      case "target":
        return t("testLog.steps.target", { host: kind.host, port: kind.port });
      case "database":
        if (kind.file) return t("testLog.steps.file", { engine: kind.engine, database: kind.database });
        return kind.database
          ? t("testLog.steps.database", { engine: kind.engine, database: kind.database, user: kind.user })
          : t("testLog.steps.databaseNoName", { engine: kind.engine, user: kind.user });
    }
  };
}

function elapsed(ms: number | null | undefined): string {
  if (ms == null) return "";
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toFixed(1)} s`;
}

/** One line for the form's footer: testing, OK (as `okText` says), the failed step, or stopped. */
export function TestSummary(props: { steps: TestStepItem[] | null; report: TestReportItem | null; running: boolean; okText(report: TestReportItem): string }) {
  const { t } = useTranslation("connections");
  if (props.running) return <span className="muted">{t("test.testing")}</span>;
  const { report } = props;
  if (!report) return null;
  if (report.stopped) return <span className="muted">{t("testLog.stopped")}</span>;
  if (report.ok) return <span className="ok">{props.okText(report)}</span>;
  const failedAt = report.steps.findIndex((s) => s.status === "failed");
  const kind = report.steps[failedAt]?.failure?.kind ?? "other";
  return <span className="error">{t("testLog.failed", { step: failedAt + 1, kind: t(`testLog.kinds.${kind}`) })}</span>;
}

export function ConnectionTestLog(props: {
  steps: TestStepItem[];
  report: TestReportItem | null;
  running: boolean;
  onStop(): void;
  onClose(): void;
}) {
  const { t } = useTranslation("connections");
  const title = useStepTitle();
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState<string | null>(null);
  const { report } = props;
  useEffect(() => setCopied(false), [report]);

  async function copy() {
    if (!report) return;
    setCopyError(null);
    try {
      // The core's text, redacted there; nothing is added here.
      await navigator.clipboard.writeText(report.diagnostics);
      setCopied(true);
    } catch (e) {
      setCopyError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <section className="test-log" aria-label={t("testLog.title")}>
      <header>
        <strong>{t("testLog.title")}</strong>
        <span className="grow" />
        {props.running && (
          <button type="button" onClick={props.onStop}>
            {t("testLog.stop")}
          </button>
        )}
        {report && (
          <button type="button" onClick={() => void copy()}>
            {copied ? t("testLog.copied") : t("testLog.copy")}
          </button>
        )}
        {!props.running && (
          <button type="button" className="icon" aria-label={t("testLog.close")} title={t("testLog.close")} onClick={props.onClose}>
            ×
          </button>
        )}
      </header>
      <ol role="log">
        {props.steps.map((step, i) => (
          <li key={i} className={`step ${step.status}`} data-status={step.status}>
            <span className="icon" aria-label={t(`testLog.status.${step.status}`)} title={t(`testLog.status.${step.status}`)}>
              {ICONS[step.status]}
            </span>
            <span className="title">{title(step)}</span>
            <span className="elapsed muted">{elapsed(step.elapsedMs)}</span>
            {step.failure && (
              <div className="failure">
                <div className="failure-kind">{t(`testLog.kinds.${step.failure.kind}`)}</div>
                <pre className="message">{step.failure.message}</pre>
                <p className="hint">{t(`testLog.hints.${step.failure.kind}`, { defaultValue: step.failure.hint })}</p>
              </div>
            )}
          </li>
        ))}
      </ol>
      {copyError && <p className="error small">{copyError}</p>}
    </section>
  );
}
