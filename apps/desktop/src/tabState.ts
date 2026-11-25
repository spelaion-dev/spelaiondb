// A tab's connection as one state, and the questions the page asks of a tab, answered once
// (ADR-060, SDB-371). Components read a tab's connection, its run and its edits only through the
// selectors here, so the palette, the buttons, the explorer and the dialogs give the same answer
// and a change to what "busy" means is made in one place. `scripts/tab-state.test.mjs` tests
// them and fails when a component reads the raw fields outside this module.
//
// Only types are imported, so the test runs this file without React or the store. Nothing here
// decides anything the core decides: the transaction, the run state and the session's health
// are the core's (ADR-028, ADR-031, SDB-370); this only holds and reads what it said.

import type { RunStateItem, TransactionStatusItem } from "./bindings";
import type { QuestionKind } from "./dialogStore";
import type { Tab } from "./store";
import { holdsConnection, tabOnConnection, type ConnectionKey } from "./connectionIdentity.ts";

/**
 * A tab's connection. Each state carries only what exists in it: a lost session has its reason
 * and no version; a transaction and "read-only for this session" exist only while connected, so
 * they end with the session that had them.
 */
export type TabConnection =
  /** No session. `failure`: why the last attempt to connect failed, if it did. */
  | { kind: "disconnected"; failure?: string }
  | { kind: "connecting" }
  | {
      kind: "connected";
      version: string;
      /** The session's transaction as the core last told it (ADR-031); none for a session that
       *  does not follow one (the explorer's, a table data tab's). */
      transaction?: TransactionStatusItem;
      /** "Read-only for this session" (SDB-395): the core holds the barrier; this only shows it. */
      readOnly?: boolean;
    }
  /** Its session died (ADR-028): nothing runs until the user reconnects. `transactionOpen`: a
   *  transaction was open, and the server rolled it back (ADR-031). */
  | { kind: "lost"; reason: string; transactionOpen: boolean }
  /** Its connection type closed it after this many seconds without use (ADR-029). */
  | { kind: "idleClosed"; reason: string; idleSeconds: number };

/** What opening a session gave: the server's version and, for an editor tab, its transaction. */
export interface OpenedSession {
  version: string;
  transaction?: TransactionStatusItem;
}

// ------------------------------------------------------------------ transitions

export const DISCONNECTED: TabConnection = { kind: "disconnected" };
export const CONNECTING: TabConnection = { kind: "connecting" };

/** The attempt to connect failed with `detail`. */
export function connectFailed(detail: string): TabConnection {
  return { kind: "disconnected", failure: detail };
}

export function connectedWith(opened: OpenedSession): TabConnection {
  return { kind: "connected", version: opened.version, ...(opened.transaction ? { transaction: opened.transaction } : {}) };
}

/** The core said the session is gone (`sessionLost`): idle-closed when it gave a time. */
export function sessionLost(reason: string, idleSeconds: number | null, openTransaction: boolean): TabConnection {
  // Read from an event: a field left out is taken as not there, never as a value (ADR-060).
  return typeof idleSeconds === "number"
    ? { kind: "idleClosed", reason, idleSeconds }
    : { kind: "lost", reason, transactionOpen: openTransaction === true };
}

/** The core's new word on the transaction; a tab no longer connected keeps no transaction. */
export function withTransaction(connection: TabConnection, transaction: TransactionStatusItem): TabConnection {
  return connection.kind === "connected" ? { ...connection, transaction } : connection;
}

/** "Read-only for this session" on or off; only a connected session has it. */
export function withReadOnly(connection: TabConnection, readOnly: boolean): TabConnection {
  if (connection.kind !== "connected") return connection;
  const { readOnly: _previous, ...rest } = connection;
  return readOnly ? { ...rest, readOnly: true } : rest;
}

// -------------------------------------------------------------- the connection

/** The three words the tab bar's dot and the status line use: a lost or idle-closed session is
 *  shown as disconnected (with its own banner, {@link lostSession}). */
export function statusName(tab: Tab): "disconnected" | "connecting" | "connected" {
  const connection = tab.connectionState;
  switch (connection.kind) {
    case "disconnected":
    case "lost":
    case "idleClosed":
      return "disconnected";
    case "connecting":
      return "connecting";
    case "connected":
      return "connected";
    default:
      return unknownConnection(connection);
  }
}

export function isConnected(tab: Tab): boolean {
  return tab.connectionState.kind === "connected";
}

export function isConnecting(tab: Tab): boolean {
  return tab.connectionState.kind === "connecting";
}

/** It has a session open or opening (a lost one is not counted: it runs nothing). */
export function hasSession(tab: Tab): boolean {
  return tab.connectionState.kind === "connected" || tab.connectionState.kind === "connecting";
}

/** Why its session is gone, or null: the reason, the idle time when it was closed for that,
 *  and whether a transaction was rolled back with it. */
export function lostSession(tab: Tab): { reason: string; idleSeconds: number | null; transactionOpen: boolean } | null {
  const connection = tab.connectionState;
  switch (connection.kind) {
    case "lost":
      return { reason: connection.reason, idleSeconds: null, transactionOpen: connection.transactionOpen };
    case "idleClosed":
      return { reason: connection.reason, idleSeconds: connection.idleSeconds, transactionOpen: false };
    case "disconnected":
    case "connecting":
    case "connected":
      return null;
    default:
      return unknownConnection(connection);
  }
}

/** Why the last attempt to connect failed, while the tab is still disconnected after it. */
export function connectFailure(tab: Tab): string | null {
  return tab.connectionState.kind === "disconnected" ? tab.connectionState.failure ?? null : null;
}

/** The server's version while connected. */
export function serverVersion(tab: Tab): string | null {
  return tab.connectionState.kind === "connected" ? tab.connectionState.version : null;
}

/** The session's transaction while connected (ADR-031). */
export function transactionOf(tab: Tab): TransactionStatusItem | undefined {
  return tab.connectionState.kind === "connected" ? tab.connectionState.transaction : undefined;
}

/** A transaction is open on its session: the tab bar's TX badge. */
export function inTransaction(tab: Tab): boolean {
  const transaction = transactionOf(tab);
  return transaction?.kind === "followed" && transaction.inTransaction;
}

/** "Read-only for this session" is on (SDB-395). */
export function sessionReadOnly(tab: Tab): boolean {
  return tab.connectionState.kind === "connected" && tab.connectionState.readOnly === true;
}

// ---------------------------------------------------------------------- the run

/** A run of this tab is going: started by the page and not yet drawn to its end. */
export function isRunning(tab: Tab | undefined): boolean {
  return tab?.run?.running ?? false;
}

/** What its session runs, as the core last said it (SDB-370). */
export function runStateOf(tab: Tab): RunStateItem | undefined {
  return tab.runState;
}

/** A cancel is on its way: Cancel is not offered again until the run ends (SDB-370). */
export function cancelling(tab: Tab): boolean {
  return isRunning(tab) && tab.runState?.kind === "cancelling";
}

/** Busy: running, or opening its session. Nothing else may start in it, and its connection may
 *  not be deleted under it. */
export function isBusy(tab: Tab): boolean {
  return isRunning(tab) || isConnecting(tab);
}

/** Whether Run can start now, as the Run button says: not while a run goes or the session is
 *  opening. A disconnected tab can: Run connects it first. The page adds what only it knows
 *  (the database list loading, history being saved, the folder changing). */
export function canStartRun(tab: Tab): boolean {
  return !isBusy(tab);
}

// --------------------------------------------------------------------- edits

/** Row edits not applied yet, or being applied: closing or leaving would lose them. */
export function hasUnsavedEdits(tab: Tab): boolean {
  return tab.pendingEdits === true || tab.savingEdits === true;
}

/** Row edits not applied yet: closing asks first. */
export function hasPendingEdits(tab: Tab): boolean {
  return tab.pendingEdits === true;
}

/** Its edits are being applied now: the tab cannot close until they are. */
export function savingEdits(tab: Tab): boolean {
  return tab.savingEdits === true;
}

// ------------------------------------------------------------ over every tab

/** Some tab on the connection is busy ({@link isBusy}): it cannot be deleted now. */
export function connectionBusy(tabs: Tab[], connectionId: string): boolean {
  return tabs.some((tab) => tab.connectionId === connectionId && isBusy(tab));
}

/** Some tab uses the connection: it has a session on it, open or opening, or runs on it. */
export function connectionInUse(tabs: Tab[], connectionId: string): boolean {
  return tabs.some((tab) => tab.connectionId === connectionId && (hasSession(tab) || isRunning(tab)));
}

/** The connection is open, for the explorer: its own session, or a connected tab's. */
export function connectionOpen(tabs: Tab[], explorerConnected: ConnectionKey[], connection: ConnectionKey): boolean {
  return holdsConnection(explorerConnected, connection) || connectedConnections(tabs).some((id) => tabOnConnection(id, connection));
}

/** The connections a connected tab uses, in tab order (repeated when several tabs share one):
 *  what a list that marks open connections subscribes to, so a tab's run does not re-render it. */
export function connectedConnections(tabs: Tab[]): string[] {
  return tabs.flatMap((tab) => (tab.connectionId !== null && isConnected(tab) ? [tab.connectionId] : []));
}

/** The explorer's own session on the connection is open (what the data search and the
 *  explorer's actions run on). */
export function explorerSessionOpen(explorerConnected: ConnectionKey[], connection: ConnectionKey): boolean {
  return holdsConnection(explorerConnected, connection);
}

// ----------------------------------------------------------------- questions

/** Every kind of question the dialog store queues (dialogStore.ts): a new kind fails the build
 *  here until it is listed, so {@link questionOpen} cannot leave it out. */
const QUESTION_KINDS = {
  password: true,
  trust: true,
  change: true,
  variables: true,
  risk: true,
  scriptError: true,
  openTransactions: true,
  params: true,
  statements: true,
  text: true,
  choice: true,
} satisfies Record<QuestionKind, true>;

/** What {@link questionOpen} reads of the dialog store: each kind's question in view, and the
 *  core's interactions. */
export type ShownQuestions = { interactions: readonly unknown[] } & Record<QuestionKind, unknown>;

/** One of the app's questions is open on top of everything (a dialog under it leaves Escape to
 *  it): any kind of question, or one the core asked (SDB-139). */
export function questionOpen(dialogs: ShownQuestions): boolean {
  return dialogs.interactions.length > 0 || (Object.keys(QUESTION_KINDS) as QuestionKind[]).some((kind) => dialogs[kind] !== null);
}

function unknownConnection(connection: never): never {
  throw new Error(`unknown tab connection: ${JSON.stringify(connection)}`);
}
