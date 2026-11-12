// The app's questions (SDB-139). Each kind of question is a queue: asking appends a request,
// its dialog shows the first one, and answering it settles that request's promise and shows
// the next. A second question of a kind asked while the first is open (a script paused on an
// error in another tab, a second connection that needs a password) waits for its turn: it
// never replaces the first one, whose flow would then wait forever.
//
// Its only runtime import is zustand, so `node --test` loads it as it is
// (scripts/dialog-queue.test.mjs).

import { create } from "zustand";
import type { ErrorAnswerItem, FieldChangeItem, InteractionKind, RiskKind } from "./api";

export interface PasswordRequest {
  connectionName: string;
  canRemember: boolean;
  error: string | null;
  resolve(answer: { password: string; remember: boolean } | null): void;
}

export interface TrustRequest {
  workspace: string | null;
  resolve(accept: boolean): void;
}

/** Values for the user's personal `${user.…}` variables (ADR-022). */
export interface VariablesRequest {
  names: string[];
  resolve(values: Record<string, string> | null): void;
}

/** A repository connection or tunnel changed since the repository was trusted (ADR-022). */
export interface ChangeRequest {
  item: string;
  isNew: boolean;
  fields: FieldChangeItem[];
  resolve(accept: boolean): void;
}

/** A line of text, such as the name to save a query under. */
export interface TextRequest {
  title: string;
  label: string;
  initial: string;
  confirm: string;
  resolve(text: string | null): void;
}

/** A question with a few answers (save, don't save, cancel…); `null` when dismissed. */
export interface ChoiceRequest {
  title: string;
  message: string;
  choices: { id: string; label: string; primary?: boolean; danger?: boolean }[];
  resolve(choice: string | null): void;
}

/** Values for the `:name` placeholders of a script. */
export interface ParamsRequest {
  names: string[];
  /** Values used last time in this tab, to start from. */
  previous: Record<string, string>;
  resolve(values: Record<string, string> | null): void;
}

/** Tabs with an open transaction about to be closed, disconnected or quit (ADR-031), tabs
 *  whose query runs and tabs with row edits not applied, before closing them, quitting or
 *  opening another folder: asked once for all of them. */
export interface OpenTransactionsRequest {
  /** Tabs whose transaction the server rolls back. */
  tabs: string[];
  /** Tabs whose running query is cancelled. */
  running: string[];
  /** Tabs whose row edits not applied yet are discarded. */
  unsaved: string[];
  action: "close" | "disconnect" | "quit" | "folder";
  resolve(proceed: boolean): void;
}

/** A run paused after an error (ADR-035): the core waits for the answer to its ticket. */
export interface ScriptErrorRequest {
  tab: string;
  /** 1-based. */
  statement: number;
  message: string;
  remaining: number;
  resolve(answer: ErrorAnswerItem): void;
}

/** Statements the core wants confirmed before they run. */
export interface RiskRequest {
  connection: string;
  statements: { sql: string; risk: RiskKind }[];
  resolve(run: boolean): void;
}

export interface InteractionItem {
  id: number;
  request: InteractionKind;
}

/** Each kind of question and its request. */
interface Requests {
  password: PasswordRequest;
  trust: TrustRequest;
  change: ChangeRequest;
  variables: VariablesRequest;
  risk: RiskRequest;
  scriptError: ScriptErrorRequest;
  openTransactions: OpenTransactionsRequest;
  params: ParamsRequest;
  text: TextRequest;
  choice: ChoiceRequest;
}

export type QuestionKind = keyof Requests;

type Shown = { [K in QuestionKind]: Requests[K] | null };

interface DialogStore extends Shown {
  /** Every request of each kind, the one shown first. */
  queued: { [K in QuestionKind]: Requests[K][] };
  interactions: InteractionItem[];
  askOpenTransactions(tabs: string[], action: OpenTransactionsRequest["action"], running?: string[], unsaved?: string[]): Promise<boolean>;
  askText(request: Omit<TextRequest, "resolve">): Promise<string | null>;
  askChoice(request: Omit<ChoiceRequest, "resolve">): Promise<string | null>;
  askParams(names: string[], previous: Record<string, string>): Promise<Record<string, string> | null>;
  askRisks(connection: string, statements: RiskRequest["statements"]): Promise<boolean>;
  askScriptError(request: Omit<ScriptErrorRequest, "resolve">): Promise<ErrorAnswerItem>;
  askPassword(connectionName: string, canRemember: boolean, error: string | null): Promise<{ password: string; remember: boolean } | null>;
  askTrust(workspace: string | null): Promise<boolean>;
  askChange(change: Omit<ChangeRequest, "resolve">): Promise<boolean>;
  askVariables(names: string[]): Promise<Record<string, string> | null>;
  /** Answers "stop" to every script error question of these tabs, wherever it waits: their
   *  runs end with the tabs that close. */
  stopScriptQuestions(tabs: string[]): void;
  pushInteraction(item: InteractionItem): void;
  popInteraction(id: number): void;
}

type Answer<K extends QuestionKind> = Parameters<Requests[K]["resolve"]>[0];

const empty = (): DialogStore["queued"] => ({
  password: [],
  trust: [],
  change: [],
  variables: [],
  risk: [],
  scriptError: [],
  openTransactions: [],
  params: [],
  text: [],
  choice: [],
});

export const useDialogs = create<DialogStore>((set, get) => {
  /** Puts `queue` in place of a kind's requests, and its first one in view. */
  const store = <K extends QuestionKind>(kind: K, queue: Requests[K][]) =>
    set((s) => ({ queued: { ...s.queued, [kind]: queue }, [kind]: queue[0] ?? null }) as Partial<DialogStore>);

  /** The requests of `kind` waiting, the shown one first. */
  const queueOf = <K extends QuestionKind>(kind: K) => get().queued[kind] as Requests[K][];

  /** Appends a request of `kind`; its promise settles with the first answer given to it,
   *  which also takes it out of the queue (a second answer does nothing). */
  const ask = <K extends QuestionKind>(kind: K, fields: Omit<Requests[K], "resolve">): Promise<Answer<K>> =>
    new Promise((settle) => {
      let answered = false;
      const request = {
        ...fields,
        resolve: (answer: Answer<K>) => {
          if (answered) return;
          answered = true;
          store(kind, queueOf(kind).filter((r) => r !== request));
          settle(answer);
        },
      } as unknown as Requests[K];
      store(kind, [...queueOf(kind), request]);
    });

  return {
    password: null,
    trust: null,
    change: null,
    variables: null,
    risk: null,
    scriptError: null,
    openTransactions: null,
    params: null,
    text: null,
    choice: null,
    queued: empty(),
    interactions: [],
    askText: (request) => ask("text", request),
    askChoice: (request) => ask("choice", request),
    askParams: (names, previous) => ask("params", { names, previous }),
    askScriptError: (request) => ask("scriptError", request),
    askRisks: (connection, statements) => ask("risk", { connection, statements }),
    askPassword: (connectionName, canRemember, error) => ask("password", { connectionName, canRemember, error }),
    askTrust: (workspace) => ask("trust", { workspace }),
    askChange: (change) => ask("change", change),
    askVariables: (names) => ask("variables", { names }),
    askOpenTransactions: (tabs, action, running = [], unsaved = []) => ask("openTransactions", { tabs, running, unsaved, action }),
    stopScriptQuestions: (tabs) => {
      for (const question of get().queued.scriptError.filter((q) => tabs.includes(q.tab))) question.resolve("stop");
    },
    pushInteraction: (item) => set((s) => ({ interactions: [...s.interactions, item] })),
    popInteraction: (id) => set((s) => ({ interactions: s.interactions.filter((i) => i.id !== id) })),
  };
});
