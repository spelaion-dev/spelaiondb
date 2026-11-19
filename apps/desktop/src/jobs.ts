// A job the core runs for the page (SDB-369): an export, a search, an import, a backup, a test,
// a row load, a plugin install. Its id is made in Rust and arrives on the job's channel as soon
// as the job starts; the page keeps no counter, and gives the ticket back to stop it. A Cancel
// pressed before the ticket arrived is sent the moment it does.

import { Channel } from "@tauri-apps/api/core";
import { call, commands } from "./api";
import type { JobTicket } from "./bindings";

export interface Job {
  /** Passed as the job command's `job` argument. */
  readonly channel: Channel<JobTicket>;
  /** Stops the job (now, or as soon as its ticket arrives). Never throws. */
  cancel(): void;
}

/** A new job's channel and its Cancel. */
export function startJob(): Job {
  const channel = new Channel<JobTicket>();
  let ticket: JobTicket | null = null;
  let wanted = false;
  const send = (job: JobTicket) => void call(commands.cancelJob(job.id, job.kind)).catch(() => undefined);
  channel.onmessage = (arrived) => {
    ticket = arrived;
    if (wanted) send(arrived);
  };
  return {
    channel,
    cancel() {
      if (ticket) send(ticket);
      else wanted = true;
    },
  };
}
