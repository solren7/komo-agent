//! Shared construction of a fully-wired `AgentRuntime`.
//!
//! Both the chat REPL (`cli/chat.rs`) and the gateway (`cli/gateway.rs`) need
//! the same agent: identical tools, skills, LLM, and reviewer. The only thing
//! that differs is the `Approver` — interactive at a TTY vs. auto-deny in the
//! unattended gateway — so it is passed in.

use komo_agent::delegate::DelegateTool;
use komo_agent::llm::{PreambleFn, build_llm};
use komo_agent::review_coordinator::ReviewCoordinator;
use komo_agent::reviewer::ReflectiveReviewer;
use komo_agent::runtime::AgentRuntime;
use komo_agent::system_prompt::SystemPromptBuilder;
use komo_core::domain::embedding::EmbeddingClient;
use komo_core::domain::skill::SkillOffer;
use komo_infra::embedding::OllamaEmbedder;
use komo_infra::memory::memory_db::MemoryDb;
use komo_infra::permissions_store::PermissionsStore;
use komo_infra::persistence::{db::Db, kanban::KanbanDb};
use komo_infra::skills::FsSkillStore;
use komo_services::clarify::ClarifyState;
use komo_services::memory_enrichment::MemoryEnricher;
use komo_services::skill_registry::SkillRegistry;
use komo_services::tool_execution::{ToolExecutionConfig, ToolExecutor};
use komo_services::tool_output_store::ToolOutputStore;
use komo_tools::apply_patch::ApplyPatchTool;
use komo_tools::ask_user::AskUserTool;
use komo_tools::cron::CronTool;
use komo_tools::edit::EditTool;
use komo_tools::glob::GlobTool;
use komo_tools::grep::GrepTool;
use komo_tools::homeassistant::HomeAssistantTool;
use komo_tools::logs::LogsTool;
use komo_tools::mcp::McpTool;
use komo_tools::memory::MemoryTool;
use komo_tools::read::ReadTool;
use komo_tools::reminder::ReminderTool;
use komo_tools::session::SessionTool;
use komo_tools::shell::ShellTool;
use komo_tools::skill::SkillTool;
use komo_tools::task::TaskTool;
use komo_tools::time::TimeTool;
use komo_tools::todo::TodoTool;
use komo_tools::web_fetch::WebFetchTool;
use komo_tools::web_search::WebSearchTool;
use komo_tools::wiki_index::WikiIndexTool;
use komo_tools::wiki_read::WikiReadTool;
use komo_tools::wiki_search::WikiSearchTool;
use komo_tools::write::WriteTool;
use std::sync::Arc;

use crate::domain::{
    approval::Approver, cron::CronJobRepository, llm::LlmClient, memory::MemoryRepository,
    repository::SkillRepository, reviewer::Reviewer, tool::Tool, workspace::Workspace,
};
use komo_config::ConfigSnapshot;

/// A wired agent plus the handles background work needs (sessions for sweeping,
/// the reviewer the sweep invokes).
pub struct Wiring {
    pub runtime: AgentRuntime,
    /// The shared review coordinator (post-turn + scheduled), for the
    /// gateway's `ReviewSweep`.
    pub review: Arc<ReviewCoordinator>,
    /// The auxiliary (cheaper) LLM, reused by the daily briefing sweep.
    pub aux_llm: Arc<dyn LlmClient>,
    /// The markdown memory store, also read by the briefing sweep.
    pub memories: Arc<dyn MemoryRepository>,
    /// The governed skill store (`~/.komo/skills`, files — roadmap §9), shared
    /// with the gateway's api channel.
    pub skills: Arc<FsSkillStore>,
    /// Mid-turn clarify state: the `ask_user` tool waits on it; the gateway
    /// dispatcher (and the TUI) resolve an inbound message into it.
    pub clarify: Arc<ClarifyState>,
    /// The briefing sweep's tool-capable agent (roadmap §2): aux model over a
    /// read-only tool set, policy-gated with a deny-all inner approver — only
    /// explicit `unattended` policy rules can grant a `Risk::Normal` action.
    pub briefing_runtime: Arc<AgentRuntime>,
    /// The cron sweep's agent for `CronAction::Agent` jobs: the full tool set
    /// (unlike briefing) but the same unattended policy gating. Main model, no
    /// memory enricher.
    pub cron_runtime: Arc<AgentRuntime>,
    /// Where over-limit tool results are stored in full. Exposed so the gateway
    /// can run the retention sweep once at startup — the store re-sweeps at most
    /// hourly on its own, and this is deliberately not a cron schedule: expiring
    /// a scratch file does not need to happen on the minute.
    pub output_store: Arc<ToolOutputStore>,
    /// Note-vault handles, shared with the operator surface so `komo wiki` works
    /// while the gateway holds the index open.
    pub wiki: Option<crate::services::operator_control::actions::WikiOps>,
}

/// Construct the memory embedding backend, or `None` when it is unconfigured
/// or unreachable.
///
/// Probed once here rather than trusted, because the failure is otherwise
/// invisible: an unreachable daemon would silently drop recall back to lexical
/// matching every turn, which looks exactly like "memory just doesn't work".
/// A warning, never a fatal — the same call komo makes for a missing model key
/// or a token-less HA channel. Recall keeps working without it.
async fn build_embedder(
    config: Option<&komo_config::EmbeddingConfig>,
) -> Option<Arc<dyn EmbeddingClient>> {
    let config = config?;
    let embedder = match OllamaEmbedder::new(&config.url, &config.model) {
        Ok(embedder) => embedder,
        Err(error) => {
            tracing::warn!(%error, "memory embedding backend unusable — recall stays lexical");
            return None;
        }
    };
    if let Err(error) = embedder.probe().await {
        tracing::warn!(
            %error,
            url = %config.url,
            model = %config.model,
            "memory embedding backend unreachable — recall stays lexical"
        );
        return None;
    }
    tracing::info!(model = %config.model, "memory embedding backend ready");
    Some(Arc::new(embedder))
}

/// Connect the configured MCP servers and turn their allowlisted tools into
/// komo tools, built **once** and shared (`Arc`) by every executor — each
/// [`McpTool`] leaks its name and description to satisfy `Tool`'s `&'static
/// str`, so constructing them per executor would leak the same strings again.
///
/// A server that is unreachable, or that no longer offers a tool the operator
/// listed, is a warning: an optional external integration must not stop komo
/// from booting (the same call komo makes for a missing model key or a
/// token-less HA channel).
async fn build_mcp_tools(servers: &[komo_config::McpServerConfig]) -> Vec<Arc<dyn Tool>> {
    if servers.is_empty() {
        return Vec::new();
    }
    let allowlists: std::collections::BTreeMap<String, Vec<String>> = servers
        .iter()
        .map(|s| (s.name.clone(), s.tools.clone()))
        .collect();
    let clients = komo_mcp::connect_all(
        servers
            .iter()
            .map(|s| (s.name.clone(), s.url.clone(), s.token.clone()))
            .collect(),
    )
    .await;

    let mut mounted: Vec<Arc<dyn Tool>> = Vec::new();
    for client in clients {
        let server = client.server().to_string();
        let offered = match client.list_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                tracing::warn!(server = %server, %error, "mcp tools/list failed — no tools mounted");
                continue;
            }
        };
        // Empty allowlist = `all_tools = true`; config resolution rejects the
        // empty-and-not-all case, so this is never an accidental wildcard.
        let allow = allowlists.get(&server).cloned().unwrap_or_default();
        let wanted = |name: &str| allow.is_empty() || allow.iter().any(|t| t == name);

        let offered_names: Vec<String> = offered.iter().map(|t| t.name.clone()).collect();
        // A listed tool the server doesn't have is almost always a typo, and it
        // would otherwise be invisible — the model just never sees the tool.
        for missing in allow.iter().filter(|t| !offered_names.contains(t)) {
            tracing::warn!(
                server = %server,
                tool = %missing,
                available = %offered_names.join(", "),
                "mcp tool listed in config is not offered by the server"
            );
        }

        let mut names = Vec::new();
        for def in offered.into_iter().filter(|d| wanted(&d.name)) {
            let tool = Arc::new(McpTool::new(client.clone(), def));
            names.push(tool.name().to_string());
            mounted.push(tool);
        }
        tracing::info!(
            server = %server,
            mounted = names.len(),
            offered = offered_names.len(),
            tools = %names.join(", "),
            "mcp tools mounted"
        );
    }
    mounted
}

/// Build the agent against `db` (sessions/messages/etc.), `kanban` (durable
/// tasks, a separate file) and `cron_jobs` (durable scheduled jobs, ditto),
/// gating side-effecting tools through `approver`. Every setting comes from the
/// caller's one resolved `config` snapshot — wiring never re-reads config.toml,
/// the env, or `.env`.
///
/// The stores are passed in rather than opened here because Turso takes an
/// exclusive lock per file: the gateway already holds all three open and must
/// hand its own handles over.
pub async fn build(
    config: &ConfigSnapshot,
    db: Arc<Db>,
    kanban: Arc<KanbanDb>,
    cron_jobs: Arc<dyn CronJobRepository>,
    approver: Arc<dyn Approver>,
) -> anyhow::Result<Wiring> {
    // An unusable model selection (bad KOMO_* value, unknown provider,
    // missing API key) can't produce a working agent — fail here like the old
    // strict resolver did.
    config.validate_agent()?;
    let model_config = &config.runtime.model;

    // Approvals the operator chose to make durable (`a` at the prompt →
    // ~/.komo/permissions.json). The store's list is *shared* with the policy, so
    // a grant applies to the next decision without a restart.
    let permissions = Arc::new(PermissionsStore::load(&config.runtime.home));
    let interactive_policy = config
        .runtime
        .policy
        .policy
        .clone()
        .with_saved(permissions.rules());

    // Wrap the interactive approver in the configurable permission policy
    // (roadmap §3): the policy auto-allows / hard-denies per `[policy]` rules and
    // only escalates to `approver` when it says "ask". With no `[policy]` table
    // this is the empty policy — identical to the bare interactive approver.
    let approver = komo_agent::policy_approver::PolicyApprover::wrap_with_store(
        interactive_policy,
        approver,
        permissions.clone(),
    );

    // Over-limit tool output is kept in full under ~/.komo/tool-output; the model
    // gets a head+tail preview naming the file (roadmap item 10).
    let output_store = Arc::new(ToolOutputStore::new(
        config.runtime.home.join("tool-output"),
    ));

    // Mutations and shell workdirs remain confined to the current working
    // directory. Local files are readable from any directory (subject to the
    // file-read permission policy); managed tool output is retained as an
    // explicit root for session-derived workspaces as well.
    let mut readonly_roots = config.runtime.readable_roots.clone();
    readonly_roots.push(output_store.root().to_path_buf());
    let workspace = Arc::new(
        Workspace::current_dir()?
            .with_readonly(readonly_roots)
            .with_unrestricted_reads(),
    );

    // ── Shared dependencies (built once, used by every tool set) ─────────────
    // Mid-turn clarification (roadmap §7): the sentinel tool suspends the turn
    // on a question; whoever routes inbound messages (gateway dispatcher, TUI)
    // resolves the answer through this shared state.
    let clarify = Arc::new(ClarifyState::new());

    // Memories live in their own SQLite file (~/.komo/memory.db), shared by the
    // `memory` tool, the reflective reviewer, the L1 pinned injection, and the
    // briefing sweep. On first run it seeds itself from any legacy markdown
    // memories under ~/.komo/memory/ (a one-time, no-op-once-populated import).
    let memory_db = MemoryDb::connect(&config.runtime.memory_db_url).await?;
    let imported = memory_db
        .import_legacy_markdown(&config.runtime.home.join("memory"))
        .await
        .unwrap_or(0);
    if imported > 0 {
        tracing::info!(imported, "migrated legacy markdown memories into memory.db");
    }
    let memory_repo: Arc<dyn MemoryRepository> = Arc::new(memory_db);

    // The delegate tool runs a separate, tool-less sub-agent on the (optionally
    // cheaper) aux model. It gets a minimal identity-only preamble — no tools,
    // skills, or project context — rebuilt per turn like the main agent.
    let aux_config = model_config.aux_variant();
    let aux_builder = Arc::new(SystemPromptBuilder::new(&aux_config));
    let aux_preamble: PreambleFn = Arc::new(move || aux_builder.build());
    // Aux/delegate sub-agents must not be fed the user's memory library — and
    // the aux agent never gets an aux of its own (no recursion).
    let aux_llm = build_llm(&aux_config, None, aux_preamble, None, Some("aux"))?;

    // The governed skill store: `~/.komo/skills` is the komo-owned home for
    // durable skills (files, not db — roadmap §9). Reviewer proposals land in
    // its `.candidates/` for triage; a one-time import moves any skills a
    // pre-filesystem komo accumulated in komo.db into that triage pile.
    let skill_store = Arc::new(FsSkillStore::new(FsSkillStore::default_root()));
    match db.export_legacy_skills().await {
        Ok(rows) if !rows.is_empty() => match skill_store.import_legacy_db(rows) {
            Ok(0) => {}
            Ok(n) => tracing::info!(n, "imported legacy komo.db skills as candidates"),
            Err(error) => tracing::warn!(%error, "legacy skill import failed"),
        },
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "failed to read legacy db skills"),
    }

    // Skills load from, in priority order (first to define a name wins):
    //   KOMO_SKILLS_PATH (colon-separated), <workspace>/skills,
    //   <workspace>/.claude/skills, the governed ~/.komo/skills store, then the
    //   user-global ~/.agents/skills and ~/.claude/skills shared by other agents.
    let root = workspace.roots().first().cloned().unwrap_or_default();
    let skill_dirs = komo_infra::skills::runtime_skill_dirs(
        &config.runtime.skills_path,
        &root,
        skill_store.root(),
        dirs::home_dir().as_deref(),
    );
    let skills = Arc::new(SkillRegistry::load_from_dirs(&skill_dirs));

    // External MCP servers. Connected here, once, because the catalog is
    // immutable after wiring: `ToolExecutor::register` takes `Arc::get_mut`,
    // and its byte-stable ordering is what keeps the provider's prompt cache
    // valid across turns. A server that appears later cannot be added.
    let mcp_tools = build_mcp_tools(&config.runtime.mcp_servers).await;

    // One memory query service, shared by the `memory` tool's explicit search and
    // the enricher's automatic recall — that sharing is the point: a model handed
    // a memory unprompted must be able to find the same memory by asking. Built
    // before the tool set because the tool holds it.
    let mut memory_query =
        komo_services::memory_query::MemoryQueryService::new(memory_repo.clone());
    if let Some(embedder) = build_embedder(config.runtime.embedding.as_ref()).await {
        memory_query = memory_query.with_embedder(embedder);
    }
    let memory_query = Arc::new(memory_query);

    // Keep the always-on preamble small: list a bounded catalog, the rest is
    // discoverable on demand via the `skill` tool.
    //
    // Built per runtime rather than once, because the catalog is gated on what
    // *that* runtime offers: a skill restricted to another OS, or one requiring
    // a tool this runtime never registered (config-absent, or dropped by a
    // policy deny), is not worth a prompt line every turn. Offer-time only —
    // `skill` view/list and every `komo skills` command ignore the gating, so a
    // skill left out of the preamble still loads the moment it's named.
    const SKILL_CATALOG_CAP: usize = 30;
    let skills_note_for = |tool_names: &[String]| -> Option<String> {
        let catalog = skills.catalog_capped(
            SKILL_CATALOG_CAP,
            &SkillOffer::here(tool_names.iter().cloned()),
        );
        (!catalog.is_empty()).then(|| {
            format!(
                "You have skills (instruction playbooks) available. To use one, call the \
                 `skill` tool with action=view and the skill name to load its instructions, \
                 then follow them. Available skills:\n{catalog}"
            )
        })
    };

    // The full tool set, parameterized only by its approver — so the main agent
    // and the unattended cron agent share one definition and can never drift.
    // The executor owns execution policy (result cap, per-turn call budget) as
    // instance config — no process globals.
    // `delegate` is passed in rather than built here because the sub-agent it
    // runs needs a tool set of its own — built by this same closure with
    // `delegate: None`, which is the structural guard against recursion.
    // Note-vault search, only when `[wiki]` names a vault. The index opens
    // lazily, so a backend that is down *right now* — a NAS still booting, a
    // local-network permission macOS has not granted the launchd job — costs a
    // retry on the next call rather than this tool for the life of the process
    // (the catalog is frozen once this returns). Only a `[wiki]` that can never
    // work, which no amount of retrying fixes, drops the tool outright.
    let mut wiki_ops: Option<crate::services::operator_control::actions::WikiOps> = None;
    // Three tools when a vault is usable: `wiki_search` (find), `wiki_read`
    // (widen a hit into its section) and `wiki_index` (maintain). They share the
    // handles, and `wiki_index` shares the runner with the operator surface so no
    // two runs overlap. `wiki_read` needs none of them — the markdown on disk is
    // the source of truth, so it reads the vault directly and serves a note
    // edited since the last index run.
    let mut wiki_tools: Vec<Arc<dyn komo_core::domain::tool::Tool>> = Vec::new();
    if let Some(wiki) = &config.runtime.wiki {
        // Registered before the handles are built, and kept even if they fail: a
        // broken vector backend costs search, not the ability to read a note whose
        // path the user or a memory already names.
        wiki_tools.push(Arc::new(WikiReadTool::new(wiki.vault.clone())));
    }
    wiki_tools.extend(match &config.runtime.wiki {
        Some(wiki) => match wiki_handles(wiki) {
            Ok((index, embedder)) => {
                // Probed once so a wrong url still shows up at boot instead of
                // on the first search. The outcome is a diagnostic, never a
                // decision. `{:#}` prints the whole chain: the outermost context
                // alone says "not reachable", which hides whether the cause was
                // the network, auth, or a permission the daemon was never given.
                match index.get().await {
                    Ok(_) => tracing::info!(vault = %wiki.vault.display(), "wiki_search ready"),
                    Err(error) => tracing::warn!(
                        error = format!("{error:#}"),
                        "wiki index not open — wiki_search retries on each call"
                    ),
                }
                // The same index backs `komo wiki` over the operator channel —
                // the gateway holds the only handle, so the CLI has to borrow it
                // rather than open its own.
                // One runner shared by every indexing caller: this process's
                // `wiki_index` tool, `komo wiki index` over the operator
                // channel, and any cron job. Two concurrent runs over one store
                // is not merely wasteful — a rebuild resets it.
                let runner = Arc::new(komo_services::wiki_indexing::WikiIndexRunner::new(
                    index.clone(),
                    embedder.clone(),
                    wiki.vault.clone(),
                    wiki.embedding.model.clone(),
                ));
                wiki_ops = Some(crate::services::operator_control::actions::WikiOps {
                    runner: runner.clone(),
                    backend: wiki.backend.clone(),
                    collection: wiki.collection.clone(),
                    location: if wiki.backend == "server" {
                        wiki.url.clone()
                    } else {
                        wiki.data_dir.join(&wiki.collection).display().to_string()
                    },
                });
                let tools: Vec<Arc<dyn komo_core::domain::tool::Tool>> = vec![
                    Arc::new(WikiSearchTool::new(index, embedder)),
                    Arc::new(WikiIndexTool::new(runner)),
                ];
                tools
            }
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "wiki_search unavailable");
                Vec::new()
            }
        },
        None => Vec::new(),
    });

    let build_full_tools =
        |approver: Arc<dyn Approver>, delegate: Option<Arc<DelegateTool>>| -> ToolExecutor {
            let mut tools = ToolExecutor::new(
                ToolExecutionConfig::with_result_cap(model_config.max_tool_result_bytes)
                    .with_turn_budget(model_config.max_turn_result_bytes)
                    .with_call_timeout_secs(model_config.tool_timeout_secs),
            )
            .with_approver(approver.clone())
            .with_output_store(output_store.clone());
            tools.register(Arc::new(TimeTool));
            tools.register(Arc::new(ReadTool::new(workspace.clone())));
            tools.register(Arc::new(WriteTool::new(workspace.clone())));
            tools.register(Arc::new(EditTool::new(workspace.clone())));
            tools.register(Arc::new(ApplyPatchTool::new(workspace.clone())));
            tools.register(Arc::new(GrepTool::new(workspace.clone())));
            tools.register(Arc::new(GlobTool::new(workspace.clone())));
            tools.register(Arc::new(ShellTool::new(workspace.clone())));
            tools.register(Arc::new(WebFetchTool::new()));
            tools.register(Arc::new(WebSearchTool::new()));
            // Note-vault search and index maintenance, present only when
            // `[wiki]` named a usable vault (opened once above — this closure is
            // synchronous).
            for tool in wiki_tools.iter().cloned() {
                tools.register(tool);
            }
            // komo's own tracing log, so a failed tool call can be diagnosed
            // from the `tool` span in the same conversation that hit it.
            tools.register(Arc::new(LogsTool));
            tools.register(Arc::new(SessionTool::new(db.clone(), db.clone())));
            tools.register(Arc::new(ReminderTool::new(db.clone())));
            // Scheduled jobs from inside a conversation. Every mutation is gated
            // through this tool set's approver — a chat-authored job is
            // model-authored, unlike one added with `komo cron add`.
            tools.register(Arc::new(CronTool::new(cron_jobs.clone())));
            tools.register(Arc::new(TaskTool::new(kanban.clone())));
            tools.register(Arc::new(TodoTool::new(db.clone())));
            tools.register(Arc::new(AskUserTool::new(clarify.clone())));
            // Home Assistant tool, only when configured (HASS_TOKEN set).
            if let Some(ha) = &config.runtime.homeassistant_tool {
                tools.register(Arc::new(HomeAssistantTool::new(
                    ha.base_url.clone(),
                    ha.token.clone(),
                )));
            }
            tools.register(Arc::new(MemoryTool::new(
                memory_repo.clone(),
                memory_query.clone(),
            )));
            // Shared instances, not rebuilt per executor — see `build_mcp_tools`.
            for tool in &mcp_tools {
                tools.register(tool.clone());
            }
            if let Some(delegate) = delegate {
                tools.register(delegate);
            }
            tools.register(Arc::new(SkillTool::new(
                skills.clone(),
                skill_store.clone(),
            )));
            // A tool the policy denies outright never gets advertised: it would
            // otherwise cost a schema, a prompt entry, and a whole round-trip per
            // attempt, all to be refused. Runs before the catalog is read, so the
            // prompt's tool list and the model's schemas agree by construction.
            let dropped = tools.drop_policy_denied(&config.runtime.policy.policy);
            if !dropped.is_empty() {
                tracing::info!(tools = %dropped.join(", "), "tools withheld by a policy deny rule");
            }
            tools
        };

    // ── Sub-agent runtime (the `delegate` tool's worker) ─────────────────────
    // A real agent turn, not a bare completion: the full tool set, so a delegated
    // subtask can actually search/read/edit — and `delegate`'s `model` argument
    // picks which model does it (plan on one, apply on another).
    //
    // Safety comes from three places, none of them a new mechanism:
    //   - it is built WITHOUT `delegate`, so a sub-agent cannot spawn another;
    //   - it shares the **main approver**, and the parent's ambient session
    //     context is inherited (`AgentRuntime::handle_input` never overrides one),
    //     so every side effect still prompts the human in the real conversation
    //     and still resolves against the parent's workspace root;
    //   - it shares the run ledger, so each delegation is auditable on its own.
    // No memory enricher: a sub-agent is a worker, not the user's assistant.
    let subagent_tools = build_full_tools(approver.clone(), None);
    let subagent_tool_names: Vec<String> = subagent_tools
        .definitions()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let subagent_note = skills_note_for(&subagent_tool_names);
    let subagent_builder = Arc::new(
        SystemPromptBuilder::new(model_config)
            .tools(subagent_tool_names)
            .skills_note(subagent_note)
            .workspace_root(Some(root.clone())),
    );
    let subagent_preamble: PreambleFn = Arc::new(move || subagent_builder.build());
    let subagent_llm = build_llm(
        model_config,
        Some(&subagent_tools),
        subagent_preamble,
        None,
        Some("delegate"),
    )?;
    let subagent_runtime = Arc::new(AgentRuntime {
        llm: subagent_llm,
        sessions: db.clone(),
        messages: db.clone(),
        runs: db.clone(),
        tool_executor: subagent_tools,
        max_turns: model_config.max_turns,
        history_window: model_config.max_history_messages,
        // A sub-agent's transcript is scratch work, not a conversation to learn
        // from — the reviewer only ever sees the real one.
        review: None,
    });
    let delegate = Arc::new(DelegateTool::new(
        subagent_runtime,
        db.clone(),
        model_config.menu(),
        model_config.model.clone(),
    ));

    let tools = build_full_tools(approver.clone(), Some(delegate));

    // Assemble the tiered system prompt: stable identity + tool-aware guidance
    // (gated on the tools actually loaded) + skills catalog, then the workspace
    // project-instruction file, then the day-precision volatile footer. Wrapped
    // in a factory so `complete` rebuilds it per turn (per session) rather than
    // freezing the date at process start — important for the long-lived gateway.
    let tool_names: Vec<String> = tools
        .definitions()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let main_note = skills_note_for(&tool_names);
    let prompt_builder = Arc::new(
        SystemPromptBuilder::new(model_config)
            .tools(tool_names)
            .skills_note(main_note)
            .workspace_root(Some(root.clone()))
            // The main agent fields "how do I configure Komo" questions, so it
            // gets the built-in platform manual (wechat login, pairing, …).
            // Aux/delegate/briefing builders deliberately don't.
            .operations_manual()
            // …and the operator-authored user profile (~/.komo/USER.md), for the
            // same reason the aux/reviewer/briefing builders don't get it.
            .user_profile()
            // …and their machine-wide agent instructions (~/.agents/AGENTS.md),
            // shared with whatever other agents read that directory.
            .global_instructions(),
    );
    let preamble: PreambleFn = Arc::new(move || prompt_builder.build());

    // Hand the same tool instances to the LLM so the model can call them, plus
    // the memory enricher (main agent only): the memory store for pinned/recall
    // selection and the aux agent for recall screening, behind one interface.
    let enricher = Arc::new(MemoryEnricher::new(
        memory_repo.clone(),
        Some(aux_llm.clone()),
        memory_query.clone(),
    ));
    let llm = build_llm(model_config, Some(&tools), preamble, Some(enricher), None)?;
    let skill_repo: Arc<dyn SkillRepository> = skill_store.clone();
    // The seam every extracted observation goes through. It shares the query
    // service with recall, so "which existing claims might this be about" is
    // answered by the same hybrid matching that decides what gets injected.
    let consolidator = Arc::new(
        komo_services::memory_consolidation::MemoryConsolidator::new(
            memory_repo.clone(),
            aux_llm.clone(),
            memory_query.clone(),
        ),
    );
    let reviewer: Arc<dyn Reviewer> = Arc::new(ReflectiveReviewer::new(
        aux_llm.clone(),
        consolidator,
        skill_repo,
        kanban.clone(),
    ));
    // One coordinator instance shared by the runtime's post-turn trigger and
    // the gateway's scheduled sweep — that sharing is what makes its
    // per-session in-flight guard effective across the two paths.
    let review = Arc::new(ReviewCoordinator::new(
        db.clone(),
        db.clone(),
        reviewer,
        config.runtime.review_interval,
    ));

    let runtime = AgentRuntime {
        llm,
        sessions: db.clone(),
        messages: db.clone(),
        runs: db.clone(),
        // The in-house agent loop hands each round to this executor; the LLM
        // above was handed the same catalog's schemas, declaration only.
        tool_executor: tools,
        max_turns: model_config.max_turns,
        // Mirror the LLM's history window so the turn loads exactly what the
        // model will replay (no full-transcript read on long chat sessions).
        history_window: model_config.max_history_messages,
        review: Some(review.clone()),
    };

    // ── Cron agent runtime (general cron, agent mode) ────────────────────────
    // Runs `CronAction::Agent` jobs: the SAME full tool set as the main agent
    // (so a scheduled job can act — shell, git, skills), but with the briefing's
    // unattended safety model — a `PolicyApprover` over a deny-all inner, so a
    // `Risk::Normal` action passes only through an explicit `unattended = true`
    // policy rule. Main model (jobs can be arbitrarily complex), no memory
    // enricher (sweeps aren't fed the user's memory library), and the run ledger
    // is shared so every job execution is auditable via `komo run list`.
    // Deliberately `wrap`, not `wrap_with_store`: saved grants were accumulated
    // interactively and must not leak into an unattended context, where only an
    // explicit `unattended = true` config rule may grant. (The engine enforces
    // this again for a channel-less decision — two floors, on purpose.)
    let cron_approver = komo_agent::policy_approver::PolicyApprover::wrap(
        config.runtime.policy.policy.clone(),
        Arc::new(UnattendedDeny),
    );
    // No `delegate`: the sub-agent runtime carries the *interactive* approver, and
    // handing that to an unattended job mixes trust models — a cron turn has no
    // ambient session, so the sub-agent's Risk::Normal actions would be auto-denied
    // anyway, just less legibly. A cron job that needs a sub-agent should say so
    // explicitly (its own runtime with the unattended approver), not inherit one.
    let cron_tools = build_full_tools(cron_approver, None);
    let cron_tool_names: Vec<String> = cron_tools
        .definitions()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    // No operations_manual / user_profile: the cron agent is a background task
    // executor, not the user-facing assistant.
    let cron_note = skills_note_for(&cron_tool_names);
    let cron_builder = Arc::new(
        SystemPromptBuilder::new(model_config)
            .tools(cron_tool_names)
            .skills_note(cron_note)
            .workspace_root(Some(root.clone())),
    );
    let cron_preamble: PreambleFn = Arc::new(move || cron_builder.build());
    let cron_llm = build_llm(
        model_config,
        Some(&cron_tools),
        cron_preamble,
        None,
        Some("cron"),
    )?;
    let cron_runtime = Arc::new(AgentRuntime {
        llm: cron_llm,
        sessions: db.clone(),
        messages: db.clone(),
        runs: db.clone(),
        tool_executor: cron_tools,
        max_turns: model_config.max_turns,
        history_window: model_config.max_history_messages,
        review: None,
    });

    // ── Briefing runtime (roadmap §2) ────────────────────────────────────────
    // A second, deliberately small agent the BriefingSweep drives: aux model,
    // read-only tool set (no shell/file/task/memory writes), and a policy
    // approver whose inner is deny-all — there is never a human to prompt, so
    // a `Risk::Normal` action passes only through an explicit `unattended`
    // policy rule. Safe reads (web_fetch, skill view) work out of the box.
    // Sharing the run ledger (`runs: db`) makes every briefing execution
    // auditable via `komo run list`.
    // No saved grants here either — see the cron approver above.
    let briefing_approver = komo_agent::policy_approver::PolicyApprover::wrap(
        config.runtime.policy.policy.clone(),
        Arc::new(UnattendedDeny),
    );
    let mut briefing_tools = ToolExecutor::new(
        ToolExecutionConfig::with_result_cap(model_config.max_tool_result_bytes)
            .with_turn_budget(model_config.max_turn_result_bytes)
            .with_call_timeout_secs(model_config.tool_timeout_secs),
    )
    .with_approver(briefing_approver.clone())
    .with_output_store(output_store.clone());
    briefing_tools.register(Arc::new(TimeTool));
    briefing_tools.register(Arc::new(WebFetchTool::new()));
    briefing_tools.register(Arc::new(WebSearchTool::new()));
    briefing_tools.register(Arc::new(SkillTool::new(
        skills.clone(),
        skill_store.clone(),
    )));
    if let Some(ha) = &config.runtime.homeassistant_tool {
        briefing_tools.register(Arc::new(HomeAssistantTool::new(
            ha.base_url.clone(),
            ha.token.clone(),
        )));
    }
    briefing_tools.drop_policy_denied(&config.runtime.policy.policy);
    let briefing_tool_names: Vec<String> = briefing_tools
        .definitions()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let briefing_note = skills_note_for(&briefing_tool_names);
    let briefing_builder = Arc::new(
        SystemPromptBuilder::new(&aux_config)
            .tools(briefing_tool_names)
            .skills_note(briefing_note),
    );
    let briefing_preamble: PreambleFn = Arc::new(move || briefing_builder.build());
    // No memory enricher: sweeps must not be fed the user's memory library.
    let briefing_llm = build_llm(
        &aux_config,
        Some(&briefing_tools),
        briefing_preamble,
        None,
        Some("briefing"),
    )?;
    let briefing_runtime = Arc::new(AgentRuntime {
        llm: briefing_llm,
        sessions: db.clone(),
        messages: db.clone(),
        runs: db.clone(),
        tool_executor: briefing_tools,
        // A briefing is an aggregation read, not a long-running job.
        max_turns: BRIEFING_MAX_TURNS,
        history_window: model_config.max_history_messages,
        review: None,
    });

    Ok(Wiring {
        runtime,
        review,
        aux_llm,
        memories: memory_repo,
        skills: skill_store,
        clarify,
        briefing_runtime,
        cron_runtime,
        output_store,
        wiki: wiki_ops,
    })
}

/// Round budget for the briefing runtime: enough for "list skills → load one →
/// fetch its data → compose", never a long-running loop.
const BRIEFING_MAX_TURNS: usize = 4;

/// Inner approver for the unattended briefing runtime: there is never a human
/// watching, so anything the policy didn't explicitly grant is denied.
struct UnattendedDeny;

#[async_trait::async_trait]
impl Approver for UnattendedDeny {
    async fn decide(
        &self,
        request: &crate::domain::approval::ApprovalRequest,
    ) -> crate::domain::approval::Decision {
        tracing::warn!(summary = %request.summary,
            "briefing: denied (unattended; add an `unattended = true` policy rule to grant)");
        crate::domain::approval::Decision::deny_because(
            "这是无人值守的后台任务，没有人能批准这一步。只有配置了 \
             `unattended = true` 的 [policy] 允许规则才会放行；请改用不需要审批的做法。",
        )
    }
}

/// Build the note-vault handles: a lazily-opened index and its embedding client.
///
/// Neither touches the network here, so the only failures left are the ones a
/// running process can never recover from — a backend name that does not parse,
/// an embedding url that is not a url. Reaching the vault is deferred to
/// [`komo_wiki::lazy::LazyWikiIndex`], which retries it per call.
fn wiki_handles(
    wiki: &komo_config::WikiConfig,
) -> anyhow::Result<(
    Arc<komo_wiki::lazy::LazyWikiIndex>,
    Arc<dyn komo_core::domain::embedding::EmbeddingClient>,
)> {
    let index = komo_wiki::lazy::LazyWikiIndex::new(komo_wiki::WikiSettings {
        backend: komo_wiki::WikiBackend::parse(&wiki.backend)?,
        data_dir: wiki.data_dir.clone(),
        url: wiki.url.clone(),
        collection: wiki.collection.clone(),
        // Credentials come from the environment, never config.toml.
        api_key: std::env::var("QDRANT_API_KEY").ok(),
    });
    let embedder = komo_infra::embedding::OllamaEmbedder::new(
        wiki.embedding.url.clone(),
        wiki.embedding.model.clone(),
    )?;
    Ok((Arc::new(index), Arc::new(embedder)))
}
