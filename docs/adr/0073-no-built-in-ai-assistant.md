# ADR-073: No built-in AI assistant: the user's own agent, through MCP, stays the way

- **Status:** Accepted
- **Date:** 2026-11-24
- **Builds on:** ADR-008 (the bet on the user's own agent through MCP), ADR-033 (the MCP page
  sets that agent up in one click), ADR-009 (secrets), ADR-071 and ADR-072 (the agents' barriers
  that the same bet keeps paying for)
- **Decides:** SDB-39

## Context
Other clients ship an assistant inside the app: a chat that turns words into SQL with the schema
as context, "explain this query", and an analysis of a plan, against OpenAI, Anthropic, Ollama
or any endpoint that speaks OpenAI's protocol, with the key in the keychain. SpelaionDB has
none. Its bet is the other direction (ADR-008): the user brings their own agent, and
`spelaiondb mcp` gives it the database behind our barriers.

The card recommended "not yet", to be looked at again once the plan viewer existed, because
explaining a plan looked like the most useful thing an assistant could do and could start as a
single, opt-in call. That condition is met, and it changes the question:

- **The plan viewer is on `dev`** (SDB-9, `docs/execution-plans.md`): a normalized tree,
  metrics, and **findings** the core computes itself (hotspots, row estimate gaps, large
  sequential scans, disk sorts…), each linked to its step. That is "explain this plan" with no
  model, the same answer every time, and tested.
- **The user's agent already gets the same plan.** `explain_query` with `structured: true`
  returns the core's tree, metrics and findings for PostgreSQL, MySQL, MariaDB, SQL Server,
  SQLite and DuckDB. An agent that is set up can explain a plan today, with the user's own
  model and account, and it can go on to look at the tables and indexes through the same tools.
- **What the agent does not get is the measured plan.** `explain_query` never runs `ANALYZE`,
  so an agent sees estimates. The person can run `ANALYZE` in the app and has the timings, and
  can save the raw plan to a file, but there is no short way to hand that plan to their agent.

So the gap left is not "the app has no model". It is the last step between what the person sees
in the app and the agent they already use.

## Decision
1. **SpelaionDB does not get a built-in AI assistant.** No provider client, no API keys of
   model providers, no chat panel, and no call from the app to a model: not for text to SQL,
   not for explaining a query, and not for explaining a plan, opt-in or otherwise. This is a
   decision not to build, not a postponement until something else exists.
2. **The user's own agent stays the way, and the app gets better at handing things to it.**
   The work goes where SpelaionDB is different from every client with a chat box: what an agent
   may do with the database (ADR-071's approval, ADR-072's function list), how it is set up
   (ADR-033), and what the MCP tools return.
3. **The app sends nothing to a model, and the person chooses what leaves.** Handing something
   to an agent is always one of two things:
   - the agent asks for it through `spelaiondb mcp`, behind the barriers and in the audit; or
   - the person copies it and pastes it where they choose, having seen what they copy.

   Two follow-ups make that shorter (see "What follows"): a copy of a plan made for pasting
   into an assistant, and prompts that the MCP server offers to the user's client.
4. **Why not, in order of weight.**
   - **It would be a second, weaker way to do what the first already does.** Text to SQL that
     is any good is not one call: it is a loop that lists objects, reads a table's columns,
     tries a query and reads the error. That loop is an agent. Built inside the app it would be
     a worse one than the user's, without their context, and outside the MCP barriers unless
     every one of them is wired a second time.
   - **Nobody can review it here.** All the code is written by agents, and the compiler, the
     tests and CI are the only reviewer (`AGENTS.md`). A test can check the plumbing of a call
     to a model. No test can check that the explanation it returns is right. The findings are
     reviewable, because they are rules.
   - **It opens the door the product is built to keep shut.** The app would hold provider keys
     and send schema, statements and plans to a third party. A plan's conditions carry the
     literals of the query, and names alone describe a company's data. Each provider is a
     network destination, a secret, a consent screen and a redaction rule that must never be
     wrong, in code that security owners would have to review.
   - **It does not end.** Providers change their APIs and retire models, users ask which model
     and why the answer was wrong, and a first call for plans becomes a request for chat. None
     of that work makes the database client better.
   - **The users it would serve are few.** It needs a provider's API key or a local model.
     Someone who has either usually has an agent, and one click sets ours up in it.

## If it is ever built
The card's hard questions get standing answers, so a later ADR starts from them:
- **What is sent, and who controls it.** Nothing leaves without the person seeing the exact
  text first, for each call (as ADR-026 and ADR-033 do for SQL and for clients' files). Never
  rows. Literals are replaced by default. The core builds the text and makes the call; the page
  never reaches the network (`connect-src` allows only Tauri's commands).
- **API keys.** In the keychain, through secret references (ADR-009), never in a file, a log,
  history or an MCP response.
- **Generated queries.** They arrive in the editor as text. Nothing a model writes runs by
  itself, and when the person runs it, it is typed SQL: the guard, the confirmations and a
  read-only connection's refusal apply as to any other statement.
- **What comes back is untrusted**, like data: shown as text, never as markup, never followed
  as an instruction.

## Options not taken
- **One opt-in call that explains a plan** (the card's way in). It is the most reasonable
  alternative: the smallest thing that could be called an assistant, on the feature most
  likely to be used. It was not taken because the findings and the agent's structured plan
  already cover it, and because it brings the whole cost of point 4 (keys, a destination,
  consent, redaction, upkeep) for one button.
- **Only a local model** (an endpoint on `localhost`, such as Ollama), so nothing leaves the
  computer. It removes the third party and the keys. It keeps the unreviewable answers and the
  upkeep, and how well a small local model reads a plan is not something our tests can tell.
- **A full assistant** (chat, text to SQL, completion), as the other clients have. See point 4.
- **Wait again** ("not yet"). The condition the card set has been met, and it weakened the
  case. Leaving the question open keeps a headline feature on the list that we do not mean to
  build.

## What would reopen this
- **People who do not use an agent ask for it, in numbers**, in reports from outside the team.
  The bet is that our users bring an agent. If that is wrong, the decision is.
- **MCP gains a standard way for an app to ask the user's own agent** a question and show the
  answer. Then an assistant in the app would be the user's agent, with their model and their
  account, and most of point 4 falls away.
- **A local model reads a plan as well as a hosted one** on an ordinary laptop. Then the
  local-only option is worth an ADR of its own, with the answers above.
- **Users choose another client because of its assistant**, said by them, not assumed by us.

## What follows (work items, created once this is accepted)
1. **Copy a plan for an assistant.** From the plan view, one action copies a compact text of
   the plan the person is looking at: the statement, the overview, the findings and the tree
   with its metrics, measured ones included when the person ran `ANALYZE`. Built by the core,
   with literals replaced unless the person asks to keep them, and shown before it is copied.
   Nothing is sent anywhere by us.
2. **Prompts in the MCP server.** `spelaiondb mcp` offers a few prompts (explain a plan,
   find why a query is slow, describe a table) that MCP clients show as their own commands.
   They only tell the agent which of our tools to call. Needs a check of which of the six
   clients of ADR-033 show a server's prompts.
3. **The public pages say it plainly:** no assistant inside, by design, and how to set up the
   user's agent in one click (`docs/guide/mcp.md`, the README).
