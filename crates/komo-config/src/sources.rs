//! Raw configuration sources: `~/.komo/config.toml`, `KOMO_*` environment
//! overrides, and `.env` credentials.
//!
//! These types carry values verbatim — precedence, defaults, and validation
//! live in `resolved.rs`. [`ConfigSources`] bundles one read of all three so
//! resolution (and every consumer of the resulting snapshot) sees a single
//! consistent view instead of re-reading the disk per setting.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::Provider;

/// One consistent read of every raw source. [`super::ConfigSnapshot::from_sources`]
/// resolves it purely — tests construct this directly instead of mutating the
/// real process environment.
pub struct ConfigSources {
    /// The `~/.komo` home directory the sources were read relative to.
    pub home: PathBuf,
    pub file: FileConfig,
    pub env: KomoEnv,
    pub secrets: Secrets,
    /// Set when the `KOMO_*` environment (or a legacy `SHION_*` fallback)
    /// failed strict parsing; its overrides are then dropped and resolution
    /// records a fatal issue.
    pub env_error: Option<String>,
}

impl ConfigSources {
    /// Read all three sources once. Never fails: a malformed `KOMO_*` value is
    /// captured in `env_error` (and later reported) instead of aborting, so
    /// diagnostic consumers like `doctor` always get a full snapshot.
    pub fn load(home: PathBuf) -> Self {
        let file = FileConfig::load(&home);
        let (env, env_error) = match KomoEnv::load() {
            Ok(env) => (env, None),
            Err(e) => (KomoEnv::default(), Some(e.to_string())),
        };
        let secrets = Secrets::load();
        Self {
            home,
            file,
            env,
            secrets,
            env_error,
        }
    }
}

/// `KOMO_*` environment overrides, deserialized in one place via envy.
/// dotenvy (`main.rs`) populates the process env from `.env` files first,
/// so these see both real env vars and `.env` entries.
#[derive(Debug, Deserialize, Default)]
pub struct KomoEnv {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Comma-separated model ids the UI may switch a session to (`KOMO_MODELS`).
    pub models: Option<String>,
    pub base_url: Option<String>,
    pub aux_model: Option<String>,
    pub schedule: Option<String>,
    pub briefing_schedule: Option<String>,
    /// `KOMO_BRIEFING_SCHEDULE_ENABLED=false` kills the briefing without
    /// touching whatever cron `config.toml` declares.
    pub briefing_schedule_enabled: Option<bool>,
    pub briefing_workdays_only: Option<bool>,
    pub dream_schedule: Option<String>,
    /// `KOMO_DREAM_SCHEDULE_ENABLED=false` — same kill switch for dreaming.
    pub dream_schedule_enabled: Option<bool>,
    pub max_turns: Option<usize>,
    pub max_tool_result_bytes: Option<usize>,
    pub max_turn_result_bytes: Option<usize>,
    pub tool_timeout_secs: Option<u64>,
    pub max_history_messages: Option<usize>,
    pub max_history_bytes: Option<usize>,
    pub llm_timeout_secs: Option<u64>,
    pub review_interval: Option<usize>,
    pub skills_path: Option<String>,
}

impl KomoEnv {
    /// Strict load: a malformed value (e.g. non-numeric `KOMO_MAX_TURNS`)
    /// is an error. Use on paths that should fail fast at startup.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from_iter(std::env::vars().collect())
    }

    fn load_from_iter(vars: Vec<(String, String)>) -> anyhow::Result<Self> {
        let current_keys = vars
            .iter()
            .filter(|(key, _)| key.starts_with("KOMO_"))
            .map(|(key, _)| key.clone())
            .collect::<std::collections::HashSet<_>>();
        let legacy_vars = vars.iter().filter_map(|(key, value)| {
            let suffix = key.strip_prefix("SHION_")?;
            (!current_keys.contains(&format!("KOMO_{suffix}")))
                .then(|| (key.clone(), value.clone()))
        });
        let legacy: KomoEnv = envy::prefixed("SHION_")
            .from_iter(legacy_vars)
            .map_err(|e| anyhow::anyhow!("invalid legacy SHION_* environment variable: {e}"))?;
        let current: KomoEnv = envy::prefixed("KOMO_")
            .from_iter(vars)
            .map_err(|e| anyhow::anyhow!("invalid KOMO_* environment variable: {e}"))?;
        Ok(legacy.normalized().overlay(current.normalized()))
    }

    /// Treat empty strings as unset, so `KOMO_MODEL=` behaves like an
    /// absent variable.
    fn normalized(mut self) -> Self {
        for slot in [
            &mut self.provider,
            &mut self.model,
            &mut self.models,
            &mut self.base_url,
            &mut self.aux_model,
            &mut self.schedule,
            &mut self.briefing_schedule,
            &mut self.dream_schedule,
            &mut self.skills_path,
        ] {
            if slot.as_deref().is_some_and(|s| s.is_empty()) {
                *slot = None;
            }
        }
        self
    }

    /// Overlay explicitly configured current-name values on legacy fallbacks.
    fn overlay(mut self, current: Self) -> Self {
        macro_rules! take_current {
            ($($field:ident),+ $(,)?) => {
                $(if current.$field.is_some() {
                    self.$field = current.$field;
                })+
            };
        }
        take_current!(
            provider,
            model,
            models,
            base_url,
            aux_model,
            schedule,
            briefing_schedule,
            briefing_schedule_enabled,
            briefing_workdays_only,
            dream_schedule,
            dream_schedule_enabled,
            max_turns,
            max_tool_result_bytes,
            max_turn_result_bytes,
            tool_timeout_secs,
            max_history_messages,
            max_history_bytes,
            llm_timeout_secs,
            review_interval,
            skills_path,
        );
        self
    }
}

/// Every credential komo reads, loaded once from the environment (and thus from
/// `~/.komo/.env`, which dotenvy folds into the process env at startup) via
/// envy. Secrets live ONLY here, never in `config.toml` — keeping behavior (the
/// `*FileConfig` types) and secrets (this one) in separate types is komo's
/// security boundary, which a single merged config would erode. envy maps each
/// field to its SCREAMING_SNAKE_CASE name, so `feishu_app_id` reads `FEISHU_APP_ID`
/// — the per-channel prefixes are encoded in the field names, no `envy::prefixed`
/// call per channel. All fields are private to the config module; the channel
/// resolution in `resolved.rs` reads them directly, and external callers go
/// through [`Secrets::key`].
#[derive(Deserialize, Default)]
pub struct Secrets {
    // Provider API keys (their env vars are unprefixed).
    pub(crate) deepseek_api_key: Option<String>,
    pub(crate) openai_api_key: Option<String>,
    pub(crate) anthropic_api_key: Option<String>,
    pub(crate) openrouter_api_key: Option<String>,
    // Channel credentials (formerly the per-channel `*Env` structs).
    pub(crate) feishu_app_id: Option<String>,
    pub(crate) feishu_app_secret: Option<String>,
    pub(crate) telegram_bot_token: Option<String>,
    pub(crate) api_server_key: Option<String>,
    pub(crate) hass_token: Option<String>,
    pub(crate) hass_url: Option<String>,
}

/// Never leak credential values through debug formatting — only presence.
impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
        f.debug_struct("Secrets")
            .field("deepseek_api_key", &set(&self.deepseek_api_key))
            .field("openai_api_key", &set(&self.openai_api_key))
            .field("anthropic_api_key", &set(&self.anthropic_api_key))
            .field("openrouter_api_key", &set(&self.openrouter_api_key))
            .field("feishu_app_id", &set(&self.feishu_app_id))
            .field("feishu_app_secret", &set(&self.feishu_app_secret))
            .field("telegram_bot_token", &set(&self.telegram_bot_token))
            .field("api_server_key", &set(&self.api_server_key))
            .field("hass_token", &set(&self.hass_token))
            .field("hass_url", &set(&self.hass_url))
            .finish()
    }
}

impl Secrets {
    pub fn load() -> Self {
        envy::from_env().unwrap_or_default()
    }

    /// The API key for `provider`, treating empty strings as unset.
    pub fn key(&self, provider: Provider) -> Option<&str> {
        let slot = match provider {
            Provider::DeepSeek => &self.deepseek_api_key,
            Provider::OpenAi => &self.openai_api_key,
            Provider::Anthropic => &self.anthropic_api_key,
            Provider::OpenRouter => &self.openrouter_api_key,
            // Codex has no env API key (OAuth via ~/.codex/auth.json).
            Provider::Codex => return None,
        };
        slot.as_deref().filter(|s| !s.is_empty())
    }
}

/// Settings read from `~/.komo/config.toml`. All fields are optional;
/// absent keys fall back to `KOMO_*` env vars then built-in defaults.
/// API keys must never appear here — keep them in `~/.komo/.env`.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct FileConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Model ids a client may switch a session to (`models = ["a", "b"]`).
    /// Unset = just `model` plus `aux_model`. They all run on the one configured
    /// provider and API key, so this is a menu within that provider, not a
    /// cross-provider router.
    pub models: Option<Vec<String>>,
    pub base_url: Option<String>,
    pub aux_model: Option<String>,
    /// 5-field Unix cron expression for gateway maintenance (default: hourly).
    pub schedule: Option<String>,
    /// 5-field Unix cron expression for the daily briefing. Unset = disabled
    /// (the briefing is opt-in; e.g. `0 8 * * *` for 8am daily).
    pub briefing_schedule: Option<String>,
    /// Master switch for the briefing, independent of its cron. `false` disables
    /// it while `briefing_schedule` keeps its value — which is what lets a
    /// deployment turn the sweep off (`KOMO_BRIEFING_SCHEDULE_ENABLED=false`)
    /// without rewriting the schedule it should return to. Default true.
    pub briefing_schedule_enabled: Option<bool>,
    /// Gate the daily briefing to Chinese working days only (statutory holidays
    /// and 调休-adjusted weekends respected). Default false.
    pub briefing_workdays_only: Option<bool>,
    /// 5-field Unix cron expression for the usage-driven memory "dreaming" sweep.
    /// Unset = on by default (nightly `0 3 * * *`); set to `"off"` (or empty) to
    /// disable.
    pub dream_schedule: Option<String>,
    /// Master switch for dreaming, independent of its cron. Same role as
    /// `briefing_schedule_enabled`. Default true.
    pub dream_schedule_enabled: Option<bool>,
    /// Maximum tool-calling round-trips per user turn (default: 30).
    pub max_turns: Option<usize>,
    /// Byte cap on a tool result handed back to the LLM, a global backstop
    /// against context-window bloat (default: 16384). See
    /// `services::tool_execution` (the executor's result cap).
    pub max_tool_result_bytes: Option<usize>,
    /// Cumulative per-turn cap on tool output fed back to the model (default:
    /// 262144; `0` = unlimited). Bounds a whole tool chain, not one result — a
    /// long chain of capped results can't silently overflow the context window.
    pub max_turn_result_bytes: Option<usize>,
    /// Per-tool-call wall-clock timeout in seconds — a hung tool (a shell
    /// command waiting on stdin, a timeout-less HTTP client) fails the call
    /// instead of wedging the turn (default: 120; `0` = no timeout). See
    /// `services::tool_execution`.
    pub tool_timeout_secs: Option<u64>,
    /// Max prior messages replayed as history per turn — the global backstop
    /// against an ever-growing chat session sending its whole transcript to the
    /// model every turn (default: 50; `0` = unlimited). See
    /// `infra::llm::RigLlm::assemble`.
    pub max_history_messages: Option<usize>,
    /// Byte budget for the replayed history per turn (default: 262144; `0` =
    /// unlimited). The companion bound to `max_history_messages`: a count window
    /// cannot stop a few pasted logs or diffs from overflowing the context.
    pub max_history_bytes: Option<usize>,
    /// Per-completion timeout in seconds — a hung provider request fails the
    /// turn cleanly instead of wedging it in `running` (default: 180; `0` =
    /// no timeout). See `infra::llm::RigLlm`.
    pub llm_timeout_secs: Option<u64>,
    /// Existing directories the agent may read in addition to its session
    /// workspace. They remain read-only: writes and shell workdirs stay inside
    /// the workspace selected when the session was created.
    pub readable_roots: Option<Vec<std::path::PathBuf>>,
    /// Ingress channel declarations (`[channels.*]` tables), shaped after
    /// hermes-agent's per-platform config blocks.
    pub channels: Option<ChannelsFileConfig>,
    /// Configurable permission policy (`[policy]` + `[[policy.rule]]`).
    pub policy: Option<PolicyFileConfig>,
    /// Long-term memory knobs (`[memory]`) — currently the embedding backend
    /// that gives L3 recall its cross-language arm.
    pub memory: Option<MemoryFileConfig>,
    /// External MCP servers (`[mcp.servers.<name>]`) whose tools are mounted
    /// into the catalog at startup.
    pub mcp: Option<McpFileConfig>,
    /// Note-vault search (`[wiki]`).
    pub wiki: Option<WikiFileConfig>,
}

/// `[mcp]` namespace: external Model Context Protocol servers.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct McpFileConfig {
    /// `[mcp.servers.<name>]` tables. A `BTreeMap` so the resolved order is
    /// the operator's alphabetical order rather than the file's — tool schemas
    /// are serialized into every request and a provider prompt cache matches on
    /// exact bytes, so a stable order is worth having for free.
    pub servers: std::collections::BTreeMap<String, McpServerFileConfig>,
}

/// One `[mcp.servers.<name>]` table.
///
/// Credentials never live here: `token_env` names the variable (in
/// `~/.komo/.env`) that holds the personal access token.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct McpServerFileConfig {
    /// Set `false` to keep the table but stop mounting the server. Declaring
    /// the table at all is the opt-in, so this defaults to **true** — unlike
    /// `[channels.*]`, which `komo init` scaffolds and which therefore has to
    /// default off.
    pub enabled: Option<bool>,
    /// The server's Streamable HTTP endpoint, e.g. `https://memos.example.com/mcp`.
    pub url: String,
    /// Name of the env var holding the bearer token. Omit for a server that
    /// needs no authentication.
    pub token_env: Option<String>,
    /// Which of the server's tools to mount. **Required** unless `all_tools`:
    /// a server can advertise dozens, every mounted schema is re-sent on every
    /// round, and a catalog that doubles hurts the model's tool selection.
    /// Closed by default.
    pub tools: Vec<String>,
    /// Mount everything the server advertises, ignoring `tools`. The escape
    /// hatch for a small server where enumerating is just noise.
    pub all_tools: bool,
}

impl FileConfig {
    /// Load from `<home>/config.toml`.
    ///
    /// - File absent → `Default` (not an error).
    /// - Parse error → warn on stderr, return `Default`, **leave file untouched**.
    pub fn load(home: &Path) -> FileConfig {
        let path = home.join("config.toml");
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return FileConfig::default();
            }
            Err(e) => {
                eprintln!("komo: could not read {}: {e}", path.display());
                return FileConfig::default();
            }
        };
        match toml::from_str(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "komo: {} is invalid (falling back to defaults): {e}",
                    path.display()
                );
                FileConfig::default()
            }
        }
    }
}

/// `[memory]` table: long-term memory behavior.
///
/// The embedding backend is what lets L3 recall cross a language boundary —
/// purely lexical recall cannot match a Chinese question against an English
/// memory, because the two tokenize into disjoint term sets. Off by default:
/// it depends on a local Ollama daemon, and recall degrades to its
/// lexical-only behavior when absent.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct MemoryFileConfig {
    /// Ollama model serving embeddings (e.g. `qwen3-embedding:0.6b`). Unset or
    /// empty = embeddings off, recall stays lexical-only. Pick a *multilingual*
    /// model — an English-only one reintroduces the very gap this closes.
    pub embedding_model: Option<String>,
    /// Ollama base URL (default `http://127.0.0.1:11434`).
    pub embedding_url: Option<String>,
}

/// `[wiki]` table: vector search over the operator's note vault. Absent, or
/// present without `vault`, means the feature is off.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct WikiFileConfig {
    /// Vault root, e.g. `~/notes`. The switch for the whole feature.
    pub vault: Option<String>,
    /// `edge` (in-process, default) or `server` (a Qdrant instance).
    pub backend: Option<String>,
    /// Qdrant gRPC endpoint, `server` backend only.
    pub url: Option<String>,
    /// Collection name; shared by both backends so an index stays portable.
    pub collection: Option<String>,
    /// Embedding model for the vault. Absent = inherit `[memory]`'s, which keeps
    /// one model resident instead of two. Set it when the vault warrants a
    /// bigger model than recall does.
    pub embedding_model: Option<String>,
    /// Embedding base URL; absent = inherit `[memory]`'s.
    pub embedding_url: Option<String>,
}

/// `[policy]` table: the configurable permission layer (roadmap §3). Parsed into
/// a `domain::policy::Policy` in `resolved.rs`.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct PolicyFileConfig {
    /// Fallback for a `Risk::Normal` action no rule matches: `ask` (default,
    /// = current behavior), `deny`, or `allow`. `Risk::Dangerous` always asks
    /// unless an explicit `include_dangerous` allow rule grants it.
    pub default_normal: Option<String>,
    /// `[[policy.rule]]` entries, evaluated deny-first.
    pub rule: Vec<PolicyRuleFileConfig>,
}

/// One `[[policy.rule]]` entry.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct PolicyRuleFileConfig {
    /// `shell` | `file` | `network` | `homeassistant` | `mcp` | `wiki`.
    pub category: String,
    /// `prefix` | `suffix` | `exact` | `contains`.
    #[serde(rename = "match")]
    pub matcher: String,
    /// The string compared against the action's target (command/path/host/...).
    pub value: String,
    /// `allow` | `deny`.
    pub effect: String,
    /// `file`-only: `read` | `write`. Omit = either.
    pub access: Option<String>,
    /// Channel scope (`["cli", "feishu"]`). Omit/empty = all channels.
    pub channels: Option<Vec<String>>,
    /// Let an `allow` rule grant `Risk::Dangerous` actions too (default false).
    pub include_dangerous: Option<bool>,
    /// Let an `allow` rule grant in no-session contexts too — the briefing
    /// sweep's tool-capable turn (default false). Deny rules apply everywhere
    /// regardless.
    pub unattended: Option<bool>,
}

/// `[channels]` namespace in config.toml: one optional table per transport.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ChannelsFileConfig {
    pub feishu: Option<FeishuFileConfig>,
    pub telegram: Option<TelegramFileConfig>,
    pub wechat: Option<WeChatFileConfig>,
    pub api: Option<ApiFileConfig>,
}

/// `[channels.feishu]` table. App credentials never live here — they are
/// read from `FEISHU_APP_ID` / `FEISHU_APP_SECRET` (in `~/.komo/.env`).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct FeishuFileConfig {
    pub enabled: bool,
    /// Sender open_id allowlist. Empty = anyone who can reach the bot.
    pub allow_from: Vec<String>,
    /// Whether group messages must @mention someone to be handled
    /// (default true; DMs always bypass this gate).
    pub require_mention: Option<bool>,
    /// Chat id that receives proactive output (reminders). Unset = keep
    /// the local macOS notifier.
    pub home_chat: Option<String>,
}

/// `[channels.telegram]` table. The bot token never lives here — it is read
/// from `TELEGRAM_BOT_TOKEN` (in `~/.komo/.env`).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct TelegramFileConfig {
    pub enabled: bool,
    /// Sender user-id allowlist. Empty = anyone who can reach the bot.
    pub allow_from: Vec<String>,
    /// Group chat-id allowlist: when non-empty, group messages are only handled
    /// in these chats (DMs always pass). Mirrors hermes' `allowed_chats`.
    pub allowed_chats: Vec<String>,
    /// Whether group messages must @mention the bot (default true; DMs
    /// always bypass this gate).
    pub require_mention: Option<bool>,
    /// Chat id that receives proactive output (reminders). Unset = keep
    /// the local macOS notifier.
    pub home_chat: Option<String>,
}

/// `[channels.wechat]` table: WeChat (微信) over the iLink personal-bot
/// protocol. There are no credentials here or in `.env` — login is QR-based and
/// the resulting token is stored in `~/.komo/wechat/credentials.json`
/// (provisioned once with `komo wechat login`). DM-only, so no group/mention
/// keys.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct WeChatFileConfig {
    pub enabled: bool,
    /// Sender iLink user-id allowlist. Empty = anyone who can reach the bot
    /// (still gated by pairing).
    pub allow_from: Vec<String>,
    /// iLink user id that receives proactive output (reminders). Unset = keep
    /// the local macOS notifier. Note: delivery still requires that user to
    /// have messaged the bot since the gateway started (see `infra/messaging/wechat.rs`).
    pub home_chat: Option<String>,
}

/// `[channels.api]` table: the OpenAI-compatible + dashboard HTTP API. The
/// bearer key never lives here — it is read from `API_SERVER_KEY` (in
/// `~/.komo/.env`), like the other channels' credentials.
///
/// The api channel is **always on**: the `komo` CLI (and `komo chat`) reach a
/// running gateway through it, because Turso's exclusive db lock means the CLI
/// can't open the db itself while the gateway holds it. `enabled = true` widens
/// it from the default loopback-only, ephemeral-port, CLI-only listener to an
/// **externally reachable** one on a stable `bind`/`port` (for Open WebUI / the
/// dashboard), and then `API_SERVER_KEY` is required.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ApiFileConfig {
    /// Expose the api externally on a stable address (requires `API_SERVER_KEY`).
    /// When false/absent the channel still runs, but loopback-only on an
    /// ephemeral port with an auto-generated key — enough for the local CLI.
    pub enabled: bool,
    /// Bind address (default `127.0.0.1`). Set `0.0.0.0` only behind a trusted
    /// proxy — the key is the only auth.
    pub bind: Option<String>,
    /// Listen port (default 8765 when `enabled`; an ephemeral port otherwise).
    pub port: Option<u16>,
    /// Serve a built web SPA (e.g. `apps/web/dist`) from the api channel as a
    /// same-origin fallback: static assets are public (unauthenticated, like
    /// `/health`), `/api` + `/v1` stay key-gated. Absent = no static serving.
    pub web_dir: Option<String>,
    /// Allow non-loopback (keyed, remote) callers to use interactive chat turns
    /// (`X-Komo-Interactive`) and resolve approval/clarify prompts over
    /// `/api/interactions/*`. Off by default: those are loopback-only, since a
    /// remote caller has no host operator behind the socket. `X-Komo-Trusted`
    /// (auto-approve) stays loopback-only regardless. Only meaningful with
    /// `enabled = true` (an external bind).
    pub remote_interactive: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("komo_config_test_{suffix}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_config_missing_file_yields_defaults() {
        let dir = tmp("missing");
        let cfg = FileConfig::load(&dir);
        assert!(cfg.provider.is_none());
        assert!(cfg.model.is_none());
        assert!(cfg.base_url.is_none());
        assert!(cfg.aux_model.is_none());
    }

    #[test]
    fn file_config_broken_toml_yields_defaults_and_keeps_file() {
        let dir = tmp("broken");
        let path = dir.join("config.toml");
        fs::write(&path, "not valid toml = = =").unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let cfg = FileConfig::load(&dir);
        assert!(cfg.provider.is_none());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "file must not be modified"
        );
    }

    #[test]
    fn file_config_loads_provider_and_model() {
        let dir = tmp("valid");
        fs::write(
            dir.join("config.toml"),
            "provider = \"openai\"\nmodel = \"gpt-4o\"\n",
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.provider.as_deref(), Some("openai"));
        assert_eq!(cfg.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn file_config_loads_max_turns() {
        let dir = tmp("max_turns");
        fs::write(dir.join("config.toml"), "max_turns = 50\n").unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.max_turns, Some(50));
    }

    #[test]
    fn file_config_loads_schedule() {
        let dir = tmp("schedule");
        fs::write(dir.join("config.toml"), "schedule = \"*/30 * * * *\"\n").unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.schedule.as_deref(), Some("*/30 * * * *"));
    }

    #[test]
    fn file_config_loads_briefing_schedule() {
        let dir = tmp("briefing");
        fs::write(
            dir.join("config.toml"),
            "briefing_schedule = \"0 8 * * *\"\n",
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        assert_eq!(cfg.briefing_schedule.as_deref(), Some("0 8 * * *"));
    }

    #[test]
    fn file_config_loads_feishu_channel_table() {
        let dir = tmp("feishu");
        fs::write(
            dir.join("config.toml"),
            concat!(
                "[channels.feishu]\n",
                "enabled = true\n",
                "allow_from = [\"ou_a\", \"ou_b\"]\n",
                "require_mention = false\n",
                "home_chat = \"oc_home\"\n",
            ),
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        let feishu = cfg
            .channels
            .and_then(|c| c.feishu)
            .expect("feishu table should parse");
        assert!(feishu.enabled);
        assert_eq!(feishu.allow_from, vec!["ou_a", "ou_b"]);
        assert_eq!(feishu.require_mention, Some(false));
        assert_eq!(feishu.home_chat.as_deref(), Some("oc_home"));
    }

    #[test]
    fn file_config_feishu_defaults_are_lenient() {
        let dir = tmp("feishu_defaults");
        fs::write(
            dir.join("config.toml"),
            "[channels.feishu]\nenabled = true\n",
        )
        .unwrap();
        let cfg = FileConfig::load(&dir);
        let feishu = cfg.channels.and_then(|c| c.feishu).expect("table parses");
        assert!(feishu.allow_from.is_empty());
        assert_eq!(feishu.require_mention, None);
        assert_eq!(feishu.home_chat, None);
    }

    #[test]
    fn api_keys_maps_provider_and_filters_empty() {
        let keys = Secrets {
            deepseek_api_key: Some("sk-x".into()),
            openai_api_key: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(keys.key(Provider::DeepSeek), Some("sk-x"));
        assert_eq!(keys.key(Provider::OpenAi), None, "empty string = unset");
        assert_eq!(keys.key(Provider::Anthropic), None);
    }

    #[test]
    fn komo_env_normalizes_empty_strings_to_unset() {
        let env = KomoEnv {
            provider: Some("openai".into()),
            model: Some(String::new()),
            ..Default::default()
        }
        .normalized();
        assert_eq!(env.provider.as_deref(), Some("openai"));
        assert_eq!(env.model, None);
    }

    #[test]
    fn komo_env_values_override_legacy_fallbacks() {
        let legacy = KomoEnv {
            provider: Some("deepseek".into()),
            model: Some("legacy-model".into()),
            max_turns: Some(10),
            ..Default::default()
        };
        let current = KomoEnv {
            provider: Some("openai".into()),
            max_turns: Some(30),
            ..Default::default()
        };
        let merged = legacy.overlay(current);
        assert_eq!(merged.provider.as_deref(), Some("openai"));
        assert_eq!(merged.model.as_deref(), Some("legacy-model"));
        assert_eq!(merged.max_turns, Some(30));
    }

    #[test]
    fn current_env_shadows_a_malformed_legacy_value_before_parsing() {
        let env = KomoEnv::load_from_iter(vec![
            ("SHION_MAX_TURNS".into(), "not-a-number".into()),
            ("KOMO_MAX_TURNS".into(), "30".into()),
        ])
        .unwrap();
        assert_eq!(env.max_turns, Some(30));
    }

    #[test]
    fn secrets_debug_shows_presence_not_values() {
        let keys = Secrets {
            telegram_bot_token: Some("123456:very-secret-token".into()),
            ..Default::default()
        };
        let s = format!("{keys:?}");
        assert!(
            !s.contains("very-secret-token"),
            "secret value must not appear in Debug output: {s}"
        );
        assert!(s.contains("telegram_bot_token: true"));
    }
}
