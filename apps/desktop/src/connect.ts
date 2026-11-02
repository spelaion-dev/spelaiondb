// Opens a session with whatever dialogs are needed: repository trust (ADR-009) and the
// password. Passwords go straight to the backend; they are never kept in the state.

import { call, commands } from "./api";
import { useApp, useDialogs } from "./store";

/** Returns the server version, or null if the user cancelled. Throws if it fails. */
export async function openSession(session: string, connectionId: string): Promise<string | null> {
  const app = useApp.getState();
  const dialogs = useDialogs.getState();
  const name = app.connections.find((c) => c.id === connectionId)?.name ?? connectionId;

  let outcome = await call(commands.connect(session, connectionId, null, false));
  if (outcome.kind === "untrusted") {
    const workspace = app.workspace?.workspace ?? null;
    if (!(await dialogs.askTrust(workspace))) return null;
    useApp.getState().setWorkspace(await call(commands.trustWorkspace()));
    outcome = await call(commands.connect(session, connectionId, null, false));
  }
  // Only ask again if the server rejected the password; any other failure (server down,
  // tunnel, TLS…) arrives as an exception and is shown as is.
  let error: string | null = null;
  while (outcome.kind === "passwordRequired" || outcome.kind === "authFailed") {
    if (outcome.kind === "authFailed") error = outcome.message;
    const answer = await dialogs.askPassword(name, outcome.canRemember, error);
    if (!answer) return null;
    outcome = await call(commands.connect(session, connectionId, answer.password, answer.remember));
  }
  if (outcome.kind === "untrusted") return null;
  return outcome.serverVersion;
}
