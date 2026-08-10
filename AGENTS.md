# AGENTS.md

Guidance for coding agents working in this repository.
`CLAUDE.md` is a symlink to this file — edit `AGENTS.md` only.

komo is a personal-agent framework in Rust (DDD-style layers) plus a bun
workspace of JS/TS clients under `apps/`. Building needs `protoc`
(`brew install protobuf` — feishu websocket frames are protobuf).

## Commands

```bash
cargo check / build / fmt
cargo test --workspace             # REQUIRED: bare `cargo test` skips komo-core's ~70 tests
cargo test tools::time             # single module

komo init                          # scaffold ~/.komo (config.toml/.env/SOUL.md/USER.md; never overwrites)
cargo run -- chat                  # full-screen TUI (needs a terminal; scripts use the api channel)
cargo run -- gateway               # always-on process: sweeps + channels (feishu/telegram/wechat/HA)
komo gateway start|stop|restart|status   # macOS launchd supervision
komo upgrade [--no-restart]        # git pull --ff-only + cargo install + restart gateway
komo logs [-n N] [-f] [--stdout]   # tail gateway tracing log
komo doctor                        # config & gateway health
komo health                        # liveness probe (exit 0 = healthy; Docker HEALTHCHECK)

komo memory list|search|promote|reject|pin|triage|report|repair-scopes
komo wiki index [--rebuild]|search|status   # note-vault index (needs `[wiki]`; index is incremental)
komo dream [--apply]               # usage-driven candidate consolidation (preview by default)
komo cron list|add|add-agent [--grant c:m:v]|run|enable|disable|remove
komo run list|inspect|resume|prune # run ledger (⟲ = recoverable)
komo skills list|install|inspect|promote|reject|protect|unprotect|enable|disable|audit
komo policy list|check|saved       # permission policy: config rules + job grants + saved grants
komo journey                       # learning timeline (memories + skills)
komo channel list|probe|setup      # channel inventory / verification / interactive setup
komo channel wechat login          # provision WeChat creds via QR (on the host)
komo pair approve|revoke|list      # admit chat senders
komo task list                     # kanban tasks
komo workday [YYYY-MM-DD]          # Chinese working-day check (holidays + 调休)
```

Logs: `init_tracing` in `main.rs` installs the subscriber (without it every
`info!` is a no-op). Gateway tees stderr into daily-rotated
`~/.komo/logs/gateway.YYYY-MM-DD.log` (what `komo logs` reads). Level via
`KOMO_LOG` (default `info,toasty=warn`; set `KOMO_LOG=debug` to see full tool
results and per-round token usage). Turns run in `run` spans, tool calls in `tool` spans,
matching the run ledger. The chat TUI logs to `~/.komo/logs/chat-tui.log`
instead (stderr would tear the alternate screen) and registers that path with
`komo_infra::logs::set_active`, which is how the `logs` tool finds the current
process's own log mid-conversation.

## Data & storage rules

| File | Contents | Durability |
|---|---|---|
| `~/.komo/state.db` | sessions, messages, todos, reminders, pairings, settings, run ledger | disposable — delete freely |
| `~/.komo/kanban.db` | cross-session tasks | durable |
| `~/.komo/memory.db` | long-term memories | durable |
| `~/.komo/cron.db` | scheduled cron jobs | durable |
| `~/.komo/permissions.json` | saved approval grants | durable |
| `~/.komo/tool-output/` | over-limit tool results (7-day retention) | disposable |
| `~/.komo/skills/` | skill files (filesystem is the source of truth) | durable |

Schema-change rules (toasty's `push_schema` runs only for **new** db files, and
is not idempotent):

- New table / non-additive change on disposable state → delete the affected
  file (`TaskRecord`→kanban.db, `CronJobRecord`→cron.db, anything else incl.
  `RunRecord`/`RunStepRecord`→state.db).
- **Column additions never need a reset**: `komo-infra/src/persistence/mod.rs::ensure_columns`
  ALTERs in place on connect. Extend `EXPECTED` in `memory_db.rs` for
  `MemoryRecord` columns, and the matching list in `db.rs::connect` for
  state.db (`SESSION_COLUMNS` / `MESSAGE_COLUMNS` / `RUN_COLUMNS` /
  `STEP_COLUMNS`). Columns must be NOT NULL + DEFAULT, or nullable.
  Durable data (memory.db) must **only** ever change additively.

Turso/toasty invariants (`komo-infra`'s `persistence/`, `memory/memory_db.rs` —
the only places the ORM appears; model structs private to their file):

- Backend is Turso in MVCC `concurrent_writes` mode; no `rusqlite`. DB URL is
  `turso:<path>` / `turso::memory:`.
- MVCC rejects `AUTOINCREMENT` → every key is a `String` UUIDv7, never `#[auto]`.
- Conflicting commits fail and must be retried: wrap single-write mutations in
  `with_write_retry`; multi-write sequences in a real transaction *inside*
  `with_write_retry` (rollback + clean re-run, never double-apply).
- Legacy rusqlite files auto-migrate once (staged to `.sqlite-backup`, `.turso`
  marker prevents re-migration).

## Gateway ↔ CLI coexistence

Turso holds an exclusive cross-process lock per db file. While the gateway
runs, the CLI cannot open the dbs directly — every operator action goes through
`services/operator_control/`: probe `~/.komo/gateway.json` (rendezvous file) →
route over the loopback api channel (`infra/messaging/api.rs`,
`infra/gateway_client.rs`) or fall back to direct db open. **Both paths run the
same `operator_control/actions.rs::OperatorActions`**, so business logic can't
fork — add new operator actions there, not in the CLI or api handlers.

- `komo chat` → `POST /v1/chat/completions` with `X-Komo-Trusted` (loopback
  only): side-effecting tools auto-approve for the host operator. API sessions
  are stored as `api:<uuid>` internally, while `X-Komo-Session-Id` carries the
  bare UUID; the gateway accepts the old prefixed form for compatibility.
- Cancel: `POST /api/interactions/{session}/cancel` flips the session's
  `CancelSignal`; `run_agent_loop` races every await against it. A running tool
  stops only if it claims `ToolContext::cancelled()` (shell kills its process
  group; web_fetch/web_search drop the request; fs tools deliberately run to
  completion so `apply_patch` never half-applies). Cancelled runs are Failed,
  **not** recoverable.
- api channel is loopback/ephemeral by default; `[channels.api] enabled = true`
  + `API_SERVER_KEY` widens it. `web_dir` serves the built SPA same-origin;
  `remote_interactive = true` lets keyed remote callers run interactive turns
  (`X-Komo-Trusted` stays loopback-only regardless). CORS grants loopback
  origins + Electron's `null` origin; bearer key remains the gate.

## Config

`~/.komo/config.toml` = runtime settings (provider/model/`models`/aux_model,
`schedule`, `briefing_schedule` + `briefing_workdays_only`, `dream_schedule`
(default nightly `0 3 * * *`, `"off"` disables), `[channels.*]`, `[policy]`,
`[memory]` — `embedding_model`/`embedding_url` for the Ollama backend behind
cross-language recall; no model = lexical-only —
`[wiki]` — `vault` (the note directory; absent = no `wiki_search`/`wiki_index`),
`backend` (`edge` default / `server`), `url` + `collection` for the server
backend, and its own `embedding_model`/`embedding_url` (falling back to
`[memory]`'s when unset); `QDRANT_API_KEY` lives in `.env` —
and `[mcp.servers.<name>]` — external MCP servers: `url`, `token_env` (names
the `.env` var, never the token), and a **required** `tools` allowlist
(or `all_tools = true`), closed by default because every mounted tool's schema
is re-sent every round).
`~/.komo/.env` = credentials only. Precedence: defaults < config.toml <
`KOMO_*` env. `KOMO_HOME` relocates the directory.

Resolution happens **once** in `crates/komo-config` into a `ConfigSnapshot`; problems
become `ConfigIssue`s (never abort resolution) checked by `validate_agent` /
`validate_gateway`. One deliberate warning, not a fatal: a missing model API key
(boots with `UnconfiguredLlm` that errors per call). **Never re-read config.toml
or call `std::env::var` in callers** — the only exception is `KOMO_HOME`.

Operator-authored prompt files (`agent/system_prompt.rs`, main agent only):
persona `~/.komo/SOUL.md`, profile `~/.komo/USER.md`, and **one instruction file
per scope, first found wins** — machine-wide `~/.komo/AGENTS.md` else
`~/.agents/AGENTS.md` (the latter under the real home, not `KOMO_HOME`, since
other agents share it), plus project `AGENTS.md` else `CLAUDE.md` else
`.cursorrules` from the working directory. Taking only the first match per scope
is what keeps a `CLAUDE.md`→`AGENTS.md` symlink from being injected twice. All of
them are head-capped and re-read on mtime change (no restart needed).

Channels (`[channels.feishu|telegram|wechat]`): behavior keys in
the table, credentials in `.env`. `allow_from` pre-trusts senders; everyone
else must pair (`komo pair approve <code>`; codes stored salted-hashed,
rate-limited, expire in 1h). WeChat is QR-login (creds in
`~/.komo/wechat/credentials.json`), DM-only, and can't deliver proactive output
until the user messages the bot after process start. `home_chat` is the
fallback for proactive output; a `/sethome` chat command override (db) wins.

Model menu: `models = [...]` declares what a session may switch to; entries may
be provider-qualified (`deepseek:deepseek-chat`) and `ModelConfig::menu()`
drops entries whose provider has no key (except the running `model`).
**A DeepSeek entry must name a v4-or-later model**: komo speaks only the
Responses API to DeepSeek, and the v3 models (`deepseek-chat`) have no
`/v1/responses` endpoint. Choice is
carried per turn in `X-Komo-Model`/`X-Komo-Effort`, validated against the menu,
stored on the session; `RoutingLlm` dispatches across providers. Effort levels
are per-provider (`Provider::efforts` ↔ `reasoning_params` must agree — there
is a test). **Invariant: every aux path (reviewer, delegate, recall, sweeps)
builds a synthetic `Session` with empty overrides** — that's what keeps a
conversation's model from leaking onto the aux model; preserve it when adding
aux callers.

The `codex` provider authenticates from the Codex CLI's OAuth file
(`~/.codex/auth.json`, auto-refreshed) instead of an env key, and requires
streaming — see `komo-infra/src/codex.rs`.

## Architecture

```
CLI/channel → AgentRuntime ─ run_agent_loop ─┬→ LlmClient::begin_turn → TurnDriver (ONE provider completion / round)
                                             └→ ToolExecutor::execute_round → tools   (loop until Step::Final)
                          ↘ MessageRepository · RunRepository (ledger) → Response
```

komo owns the tool loop **and its provider layer** (`crates/komo-provider`, no
LLM crate): one completion per round, `run_agent_loop` (`agent/runtime.rs`) is where
round-level control lives (`max_turns` budget, cancellation, clarify). Tool
errors return as outcome content the model can recover from; only a driver/LLM
error aborts the turn.

**Crate layout.** The lower half of the tree is split out of the binary so it
compiles in parallel and so an edit there does not rebuild everything (`src/` was
one 50k-line crate). Depend downward only:

```
komo-core      traits + value types, no I/O, no runtime — the GUI client reuses it
komo-config    config.toml + .env + KOMO_* → one ConfigSnapshot   (→ core)
komo-provider  wire formats + HTTP/SSE; references nothing else in komo
komo-mcp       MCP client over rmcp (Streamable HTTP); ditto — nothing komo
komo-wiki      note-vault vector index: edge (qdrant-edge, in-process) /
               server (Qdrant over gRPC) / lazy                        (→ core)
komo-infra     persistence · memory · skills · logs · workday ·
               permissions_store · codex · embedding         (→ core, config, provider)
komo-services  tool_execution · tool_output_store · memory_enrichment · clarify ·
               skill_registry · cron_actions · wiki_indexing ·
               diff/patch/search/file_mutation                (→ core, config)
komo-tools     every tool                      (→ core, infra, mcp, services)
komo-agent     runtime · gateway · daemon · interaction · system_prompt ·
               policy_approver · reviewer · llm · delegate
                                            (→ core, config, provider, infra, services)
komo (bin)     cli · tui · `infra/messaging` (channels) · `infra/gateway_client` ·
               `services/operator_control` — the wiring layer, plus what needs
               the agent above it; each `mod.rs` says why it stayed
```

Test-only constructors a dependent crate's tests need — `persistence::reset_test_db`,
`SkillRegistry::new`, `komo-tools`' fixtures — are behind each crate's
`test-support` feature, enabled only as a dev-dependency so they never ship.

Cron scheduling math (`next_occurrence_local`) lives in `komo-core`'s
`domain::cron`, and every job mutation goes through `komo-services`'
`cron_actions` — the `cron` tool, the gateway handlers and the CLI adapter all
call the same functions, which is what keeps validation from forking.

**Module map** (one line each; read the module for details):

- `domain/` — pure traits + value types, no I/O, no external crates
  (`Tool`, `LlmClient`/`TurnDriver`, repositories, policy engine, pairing).
- `komo-agent`'s `runtime` — session lifecycle + the tool loop; loads only a recent
  transcript window per turn (`find_windowed`); wraps each turn in a ledger
  `Run` (all ledger writes best-effort, never fail the turn).
- `crates/komo-provider` — komo's own provider layer, its own crate because it
  references nothing else in komo (so it compiles in parallel with the rest).
  One module per **wire format**
  (`Wire`), not per provider: `responses` (OpenAI / Codex / DeepSeek /
  OpenRouter) and `messages` (Anthropic, which serves no Responses endpoint).
  `transport` is the HTTP+SSE boundary where `error::LlmError` is built while the
  status, headers and provider error `code` are all still intact — retryability
  is `LlmError::is_retryable()` (exhaustive match) and the server's own
  `Retry-After` beats any local backoff. Every request streams; a stream that
  ends without its terminal frame is a retryable failure, never a short answer.
  A new provider is a base URL + auth mode, not new code.
- `komo-agent`'s `llm` — `ProviderLlm` over that layer; `assemble` builds the tiered
  system prompt once per turn (stable tier incl. `~/.komo/USER.md` and the
  machine-wide instruction file, then memory
  prefix from `MemoryEnricher` — main agent only). `RoutingLlm` = cross-provider
  dispatch. Reasoning blocks are echoed back verbatim each round, which is what
  carries a reasoning model's chain of thought across a tool loop.
- `services/tool_execution/` — `ToolExecutor::execute_round`: per call, claim
  ledger seq → redact args → run with panic catch + `tool` span →
  transient-retry (connection errors retry anything; ambiguous only
  `Tool::idempotent()`) → bound the LLM-facing result via
  `services/tool_output_store.rs` (full text on disk, head+tail preview) →
  record `RunStep`. Policy is instance-owned `ToolExecutionConfig`;
  `Tool::max_duration()` overrides the per-call timeout (approval-gated tools
  must outlast the 5-min approval prompt, `APPROVAL_BOUND`).
  `Tool::call(Value, &ToolContext)` is the **only** tool entry point; the
  `SESSION` task-local serves the approvers only — tools take `ctx.session`.
- `komo-tools` — `time`, `shell` (own process group, hardline floor no approval
  unlocks, nested timeouts), `grep`/`glob` (ripgrep libraries in-process;
  policy runs over paths **before** content is read), `read`/`write` +
  `fs_common` (workspace-confined; `write_if_unchanged` guards the approval
  window), `edit` (exact match only, no fuzzy) / `apply_patch` (v2 envelope,
  one approval per batch, no rollback — reports exactly what landed),
  `web_fetch` (content-type gated, 256 KB download cap, deny-only network
  policy), `homeassistant` (`call_service` approval-gated; `BLOCKED_DOMAINS`
  hardline), `task`, `todo` (session-scoped, dies on `/new`), `memory`,
  `skill`, `cron`, `ask_user` (clarify), `logs` (tail of komo's own
  tracing log — file lookup shared with `komo logs` via `komo-infra`'s `logs`, same
  deny-only file-read gate as `read`).
- `komo-agent`'s `delegate` — sub-agent as a real agent turn on a `delegate:<uuid>`
  session; inherits the parent's ambient session context (approvals prompt the
  real conversation, cancel propagates); recursion blocked structurally
  (sub-agent tool set has `delegate: None`); each delegation is its own ledger
  run. The unattended cron runtime gets no `delegate`.
- `domain/policy.rs` + `komo-agent`'s `policy_approver` — permission policy. Ladder,
  strongest first: **tool hardline floor > config deny > saved grant > config
  allow / `default_normal` > ask**. Saved grants (`permissions.json`, written
  only by `PolicyApprover`) never cover `Risk::Dangerous` and are never read
  unattended. Unattended contexts (cron/briefing/sweeps) grant only through
  `unattended = true` allow rules **or the running job's own `grants`**
  (`CronJob.grants`, approved in the same prompt that created the job; carried
  into the turn by `with_job_grants`, scoped to that turn, revoked with the job).
  Full ladder: **tool hardline floor > config deny > job grant > saved grant >
  config allow / `default_normal` > ask**. **What marks a turn unattended is
  `SessionContext::origin`** (`SessionOrigin::Cron` / `Briefing`, set by the
  sweep that starts the turn), *not* the absence of an ambient session — those
  turns have a real session id, and reading a channel off it is what used to
  make the engine's unattended branch unreachable. Read-only actions (`read`, `web_fetch`) are
  deny-only — never prompted. Wholly-denied tools are dropped from the catalog
  at wiring (`drop_policy_denied`). Policy only tightens; hardline floors
  short-circuit inside the tool.
- `komo-mcp` + `komo-tools`' `mcp` — external MCP servers over Streamable HTTP
  (rmcp, client features only). `[mcp.servers.*]` is connected **once at
  wiring**: the catalog is immutable after that (`register` takes
  `Arc::get_mut`, and its byte-stable order is what keeps the provider prompt
  cache valid), so a server that is down at boot has no tools for the process's
  lifetime — and an unreachable one is a warning, never a fatal. Each mounted
  tool becomes `mcp__<server>__<tool>` (leaked to satisfy `Tool::name`'s
  `&'static str`; built once and `Arc`-shared across every executor). **Every
  MCP call is approval-gated** — `annotations.readOnlyHint` is server-authored,
  and the server is the party being gated; grant specific tools with
  `category = "mcp"`, `value = "<server>.<tool>"` rules. A `tools/call` that
  comes back with `isError` is returned as *content*, not a `ToolError`: the
  message is remote-controlled and the retry classifier falls back to substring
  matching, so an echoed "connection refused" must not re-fire a mutation.
- `domain/memory.rs` + `services/memory_enrichment.rs` — three surfaces:
  L1 pinned block (manual `pin` only), L2 `memory` tool + operator CLI,
  L3 recall (fetch 15, inject ≤5, aux-screened above 5;
  injected hits get `recall_count`/`last_used_at`/query-hash stamped —
  dreaming's signals). Nightly `DreamSweep` promotes candidates recalled ≥3
  times by ≥2 distinct queries, archives 30-day-cold ones; only candidates are
  touched. Reviewer extractions are always `candidate`, never pinned/active.
  L3 matching is **lexical ∪ semantic** (`RecallQuery`): shared terms, or
  cosine ≥ `RECALL_SEMANTIC_FLOOR` against the memory's embedding. The semantic
  arm is not optional polish — CJK bigrams and ASCII words can never be equal,
  so lexical-only recall structurally cannot match a Chinese question to an
  English memory. Embeddings come from `[memory] embedding_model` via
  `komo-infra`'s `embedding` (Ollama; a *multilingual* model, or the gap
  returns), are stored per memory with the model that produced them
  (`embedding_for` rejects a foreign vector), and are backfilled in the
  background by `enrich` — so every write path is covered by one implementation.
  Every embedding failure degrades to lexical, never to worse.
  **Scope**: `write_scope()` only channel-scopes a *durable* channel
  (`is_durable_channel`). The `api` platform's chat id is per-conversation
  (TUI/desktop/web all ride it), so channel-scoping there makes a memory
  unrecallable from the next turn — those writes go `Global`. Memories written
  before this are repaired by `komo memory repair-scopes`.
- `domain/wiki.rs` + `komo-wiki` + `komo-services`' `wiki_indexing` +
  `komo-tools`' `wiki_search` / `wiki_index` — semantic search over the note vault
  (`[wiki] vault`), **pulled on demand, never auto-injected** like memory recall:
  a vault dwarfs the memory store, so a turn that does not search pays nothing.
  Two interchangeable backends behind `WikiIndex`, chosen by `[wiki] backend`:
  `edge` (qdrant-edge, in-process, the default) and `server` (Qdrant over gRPC,
  for sharing one collection across processes). They speak the same data model,
  so an index built by one is readable by the other — but **nothing migrates**,
  and a switch leaves the new backend empty until `komo wiki index` refills it.
  Retrieval is hybrid (BM25 fused with dense), capped per note so one long file
  cannot crowd out a result set. `LazyWikiIndex` opens the backend on first use
  and retries per call: wiring is one-shot, so an eager open that failed would
  cost `wiki_search` for the life of the process — and the usual causes (a NAS
  still booting, a local-network permission the launchd job lacks) get fixed
  while the gateway keeps running. The gateway holds the only handle, so
  `komo wiki` borrows it through `operator_control` rather than opening its own.
  Indexing is **incremental by mtime** (embedding is the whole cost of a run, so
  an unchanged file costs nothing) and `--rebuild` is the opt-out. **Nothing
  reindexes on a schedule** — there is no wiki sweep; a cron job with a
  `wiki:exact:refresh` grant is how you get one. Every indexing caller goes
  through one `WikiIndexRunner`: `wiki_index`, `komo wiki index`, and any job.
  Its `claim` is an RAII guard, so an abandoned run frees the slot instead of
  locking indexing out for the process's life. `wiki_index`'s three actions are
  three risk levels — `status` `Safe` (the diagnosis surface: an `indexed_by`
  that differs from the configured model is *the* index anomaly), `refresh`
  `Normal` and synchronous, `rebuild` `Dangerous` and **detached**: a rebuild
  `reset()`s the store before refilling it and outlives any `max_duration`, so
  running it inside the call would let a timeout abort it with the store already
  emptied. Its outcome is read back with `status`.
- `domain/run.rs` — run ledger: one `Run` per turn, one `RunStep` per call.
  `elapsed_ms` is the duration field (`started_at`/`ended_at` are whole
  seconds); 0 / empty `structured` read as *unknown/absent*, never
  instant/empty-object. Args redacted per-tool (`Tool::redact_args`); results
  truncated not scrubbed. `komo run resume` re-dispatches a *fresh* primed
  turn (the ledger is an audit record, not a checkpoint); `recoverable` is set
  only by crash reconciliation, cleared at-most-once, never auto-resumed.
- `domain/skill.rs` + `komo-infra`'s `skills` + `services/skill_registry.rs` —
  skills are `SKILL.md` files under `~/.komo/skills/` (active) and
  `.candidates/` (proposals). Automated writes (`save` — reviewer + `skill
  learn`) only ever produce candidates; `install` is the human-in-the-loop
  exception that lands active. `protected` skills refuse even proposals.
  `SkillRegistry` re-scans dirs on every query (no restart needed); only the
  capped prompt catalog is a startup snapshot (cache stability).
- `komo-agent`'s `daemon` — `Maintenance` sweeps under `supervise` (circuit breaker
  after 5 failures): `ReviewSweep` (via the shared `ReviewCoordinator`, which
  also serves the post-turn trigger — watermark + in-flight guard prevent
  duplicate reviews), `ReminderSweep`, `CronJobSweep` (claim-before-run: a
  crash never re-fires a slot), `TaskSweep`, `BriefingSweep` (opt-in; aux-model
  runtime with read-only tools + deny-all unattended approver; degrades to
  tool-less `complete` on error), `DreamSweep`. `WorkdayGated` decorator gates
  a sweep to Chinese working days (`komo-infra`'s `workday`, cached per-year).
- `komo-agent`'s `gateway` + `interaction` — gateway hosts channels +
  sweeps. `GatewayDispatcher` owns turns (spawned per turn so `/approve` can
  arrive mid-turn; one turn per session). Chat commands: `/new` (rotate
  session, clear todos + approval state), `/approve [session|always]`,
  `/deny`, `/sethome`, `/wechat login`. `ChatApprover` suspends the turn on a
  oneshot (5-min timeout); no session in context ⇒ deny. `HomeNotifier`
  delivers all proactive output (sethome override > config `home_chat`,
  feishu first > macOS notification).
- `infra/messaging/` — channels: feishu (ws long connection on a dedicated
  thread), telegram (long polling, Markdown with plain-text fallback), wechat
  (iLink, DM-only, shared `WeChatBot` instance, in-memory reply tokens).
  Session ids: `{platform}:{chat_id}`. Home Assistant is **not** a channel —
  it is reachable only through the `homeassistant` tool (agent pulls on
  demand); recurring device reactions belong in an HA automation written via
  the tool's `save_automation`, not in an event stream that costs an LLM turn
  per sensor tick.
- `cli/wiring.rs` — shared `AgentRuntime` construction (chat vs gateway differ
  only in `Approver`); register new tools here.
- `tui/` — ratatui chat front end over gateway-or-in-process backends; state +
  key handling terminal-free in `tui/app.rs`. `komo resume <id>` (or the
  compatible `komo session resume <id>`) re-enters a session; a bare API UUID
  resolves its internal `api:<uuid>` id and hydrates the transcript. Input:
  Enter sends, Shift/Alt-Enter (kitty protocol) or Ctrl-J newline, **Esc stops
  the turn in flight** (nothing when idle — a stop key that sometimes discards the
  draft is worse than one extra keystroke; under the approval modal Esc keeps
  meaning "deny"). Local turns carry a `CancelState` signal on their
  `SessionContext`; remote turns cancel over
  `POST /api/interactions/{session}/cancel`, which also denies a pending approval
  and answers a pending `ask_user` — a turn parked on either never reaches
  another await, so the signal alone would not reach it. `tui/paste.rs`
  holds both paste mechanisms — a chip folds a ≥4-line / >10 KB paste to a label
  (`input` still holds the full text; the chip's byte range is what keeps
  rendering off the folded content) and `coalesce_rapid_keys` rebuilds a paste
  that a terminal without bracketed paste delivered as keystrokes. Input events
  go through a channel so a batch can be collected before it is interpreted.
- `cron` (`~/.komo/cron.db`, `CronJobSweep`) — two job modes: **command**
  (operator-authored, runs directly, no approver) and **agent** (unattended
  turn on `cron_runtime`, side effects need `unattended = true` policy rules).
  Chat-created jobs (`tools/cron.rs`) are approval-gated at creation; a
  command job from chat is `Risk::Dangerous`. An agent job declares the actions
  it needs as `grants`, approved in that **same** prompt (which is why a
  grant-carrying `add` drops the `cron:add` scope key) — narrower than a global
  `unattended` rule and revoked when the job is removed. Recurring *work* = cron job,
  recurring *message* = reminder.
- `apps/` — bun workspace: `apps/app` (shared React renderer) mounted by
  `apps/desktop` (Electron) and `apps/web` (SPA served via `web_dir`). Talks
  to the gateway over HTTP only (`HttpKomoClient`); feature-first layout;
  react-query for server state, zustand for client state; thread is
  assistant-ui over an async-generator adapter. Components may only use
  semantic theme tokens — `bun run lint` fails on raw colors. Commands:
  `cd apps && bun install`, `bun run check` (typecheck + lint + fmt + test).
  Conventions: `apps/app/README.md`.

## Extension points

- **Add a tool**: implement `Tool` in `crates/komo-tools/src/`, register in `cli/wiring.rs`
  (and add it to `tool_execution::policy_scope` if it should be policy-filterable).
- **Add an MCP server**: config only — an `[mcp.servers.<name>]` table with a
  `tools` allowlist. No code; that is the point of `komo-mcp` being generic.
- **Swap LLM provider**: implement `LlmClient` (`domain/llm.rs`), construct in
  `komo-agent`'s `llm::build_llm`.
- **Swap persistence**: implement the repository traits; `agent/`/`domain/`
  need no changes.
- **Add a provider**: an entry in `Provider` plus its base URL / auth / wire in
  `infra/llm.rs` (`wire_for`, `endpoint_url`, `build_provider_llm`). A new *wire
  format* — only if it speaks neither Responses nor Messages — is a module in
  `crates/komo-provider` and a `Wire` variant.
- **Agent-loop control**: add round-level control points in `komo-agent`'s `run_agent_loop`;
  extend `TurnDriver`/`Step`. Clarify (`tools/ask_user.rs` +
  `services/clarify.rs`) is the sentinel-tool reference.
- **Scheduled action**: implement `Maintenance`, construct in `cli/gateway.rs`.
- **Gateway ingress**: implement `Channel`, `add_channel` in `cli/gateway.rs`,
  gate behind a `[channels.*]` declaration — feishu is the reference.

## Testing

Tests live beside the code (`#[cfg(test)] mod tests`, `#[tokio::test]` for
async), named by behavior. **Always `cargo test --workspace`** — the bare root
command skips `crates/komo-core`.

## Coding style

`cargo fmt` defaults; `snake_case` modules/functions, `PascalCase` types. Small
modules, one responsibility; keep async db code in the layer that owns it. CLI
subcommands short and verb-based.

## Commit & PR style

Short imperative commits (`add file tool`). PRs: concise description, commands
run for verification, terminal output when CLI behavior changes.

## Repo docs

- Issues/PRDs: local markdown under `.scratch/<feature-slug>/` — `docs/agents/issue-tracker.md`
- Triage labels: `needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix` — `docs/agents/triage-labels.md`
- Domain docs: `CONTEXT.md` + `docs/adr/` — `docs/agents/domain.md`
- Long-form design rationale (archived old AGENTS.md): `docs/agents/architecture-notes.md`
