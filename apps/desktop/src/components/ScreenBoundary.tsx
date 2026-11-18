// When a screen fails to render (SDB-398): a panel in its place instead of a blank window, with
// what failed, "Reload this screen", "Copy report" (written and scrubbed by the core) and the
// GitHub issue page, opened only on a click. The rest of the app keeps working.

import { Component, useState, type ErrorInfo, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { copyDiagnostics, openIssuePage } from "../diagnostics";

interface Props {
  /** Which screen this is, in the report: `explorer`, `tab`, `settings`… */
  screen: string;
  /** Hidden as the screen it stands for would be. */
  hidden?: boolean;
  /** The class of the panel's box, so it fills the screen's place. */
  className?: string;
  children: ReactNode;
}

interface State {
  error: Error | null;
  stack: string;
  /** Bumped by "Reload this screen": the screen is mounted again from scratch. */
  attempt: number;
}

export class ScreenBoundary extends Component<Props, State> {
  state: State = { error: null, stack: "", attempt: 0 };

  static getDerivedStateFromError(error: unknown): Partial<State> {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidCatch(_error: unknown, info: ErrorInfo) {
    this.setState({ stack: info.componentStack ?? "" });
  }

  render() {
    const { error, stack, attempt } = this.state;
    if (!error) return <ScreenKey key={attempt}>{this.props.children}</ScreenKey>;
    return (
      <FailedScreen
        screen={this.props.screen}
        hidden={this.props.hidden}
        className={this.props.className}
        error={error}
        stack={stack}
        onReload={() => this.setState((s) => ({ error: null, stack: "", attempt: s.attempt + 1 }))}
      />
    );
  }
}

/** Only there to carry the key that remounts the screen. */
function ScreenKey({ children }: { children: ReactNode }) {
  return <>{children}</>;
}

function FailedScreen(props: { screen: string; hidden?: boolean; className?: string; error: Error; stack: string; onReload(): void }) {
  const { t } = useTranslation();
  const [note, setNote] = useState<{ text: string; failed: boolean } | null>(null);
  const failure = { screen: props.screen, error: `${props.error.name}: ${props.error.message}`, stack: props.stack };
  const copy = () => {
    copyDiagnostics(failure).then(
      () => setNote({ text: t("screenFailed.copied"), failed: false }),
      (e: unknown) => setNote({ text: t("screenFailed.copyError", { detail: e instanceof Error ? e.message : String(e) }), failed: true }),
    );
  };
  const report = () => {
    openIssuePage().catch((e: unknown) => setNote({ text: t("screenFailed.openError", { detail: e instanceof Error ? e.message : String(e) }), failed: true }));
  };
  return (
    <div className={`screen-failed ${props.className ?? ""}`} hidden={props.hidden} role="alert">
      <h2>{t("screenFailed.title")}</h2>
      <p>{t("screenFailed.body")}</p>
      <p className="screen-failed-error">{failure.error}</p>
      <div className="screen-failed-actions">
        <button className="primary" onClick={props.onReload}>{t("screenFailed.reload")}</button>
        <button onClick={copy}>{t("screenFailed.copy")}</button>
        <button onClick={report}>{t("screenFailed.report")}</button>
      </div>
      <p className="muted small">{t("screenFailed.privacy")}</p>
      {note && <p className={note.failed ? "error small" : "small"} role="status">{note.text}</p>}
    </div>
  );
}
