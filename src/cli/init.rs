//! `komo init` — bootstrap the config home with commented templates.
//!
//! Writes a default `config.toml`, a `.env` credential template, and a
//! default `SOUL.md` persona into `~/.komo/` (or `KOMO_HOME`). Existing files
//! are never touched, so the command is safe to re-run and safe inside a
//! running gateway (pure file ops, no db). Pairs with the degraded no-API-key
//! startup: a fresh install boots, `komo init` scaffolds the files, the
//! operator fills in a key.

use std::path::Path;

/// The generated `~/.komo/config.toml`. Runtime settings only — credentials
/// belong in `.env` (see [`ENV_TEMPLATE`]). Everything but the provider line
/// is commented out at its built-in default, so the file documents itself.
const CONFIG_TEMPLATE: &str = r#"# komo runtime settings. Credentials never go here — put them in .env
# next to this file. Priority: built-in defaults < this file < KOMO_* env.

# LLM provider: deepseek | openai | anthropic | openrouter | codex
# (codex needs no API key — it uses the Codex CLI's OAuth login)
provider = "deepseek"
# model = "deepseek-chat"        # defaults per provider
# base_url = ""                  # OpenAI-compatible endpoint override
# aux_model = ""                 # cheaper model for sub-tasks (reviewer/recall/briefing)

# Maintenance sweep cron (5-field Unix cron). Default: hourly.
# schedule = "0 * * * *"

# Daily briefing — opt-in, no default. Uncomment to enable.
# briefing_schedule = "30 8 * * *"
# briefing_workdays_only = true  # skip Chinese non-working days (incl. 调休)
# briefing_schedule_enabled = false  # kill switch, keeps the cron above

# Usage-driven memory consolidation ("dreaming"). On by default, nightly.
# dream_schedule = "0 3 * * *"   # set to "off" to disable
# dream_schedule_enabled = false     # kill switch, keeps the cron above

# --- memory ------------------------------------------------------------------

# Embeddings for memory recall. Leaving this unset is not a small loss and it
# is not reported anywhere: recall keeps working, but only *lexically*, and
# lexical matching cannot cross languages — CJK bigrams and ASCII words are
# different strings by construction, so a Chinese question can never reach an
# English memory. The model must be a multilingual one, and the same one over
# time (a vector is only compared against its own model's space; changing the
# model re-embeds the library rather than mixing spaces).
#
# After setting this on an existing store, run `komo memory backfill` — recall
# embeds lazily, one batch per read, so a library written before this was
# configured would stay half-lexical for a long time.
# [memory]
# embedding_model = "qwen3-embedding:4b"   # served by ollama
# embedding_url = "http://127.0.0.1:11434"

# --- note vault (wiki) -------------------------------------------------------

# Semantic search over a directory of markdown notes. Absent = no wiki tools.
# Falls back to [memory]'s embedding settings when its own are unset.
# [wiki]
# vault = "~/notes"
# backend = "edge"               # edge = in-process; server = shared Qdrant
# url = "http://127.0.0.1:6334"  # server backend only (QDRANT_API_KEY in .env)
# collection = "komo_wiki"

# --- python plugins ----------------------------------------------------------

# On by default: the gateway creates $KOMO_HOME/plugins/ at startup (so
# ~/.komo/plugins unless KOMO_HOME moved it — `komo doctor` prints the real
# path) and runs an out-of-process python3 host. Every *.py there with an @tool decorator becomes
# a py__<name> tool (hot-reloaded on save), and run_code lets the agent write
# programs that call its own tools. Requires python3 on PATH.
# [plugins.pyhost]
# enabled = false                # opt out (also silences the python3 warning)

# --- ingress channels (each needs its credential in .env) -------------------

# [channels.telegram]
# enabled = true
# allow_from = ["123456789"]     # pre-trusted sender ids (skip pairing)
# require_mention = true         # group messages must @mention the bot
# home_chat = "123456789"        # reminders/briefing delivered here

# [channels.feishu]
# enabled = true
# allow_from = ["ou_xxx"]
# require_mention = true
# home_chat = "oc_xxx"

# [channels.wechat]              # DM-only; provision with `komo channel wechat login`
# enabled = true

# [channels.api]                 # widen the loopback HTTP API (needs API_SERVER_KEY)
# enabled = true
# bind = "0.0.0.0"
# port = 8765
"#;

/// The generated `~/.komo/.env`. Credentials only; empty values read as
/// unset, so the uncommented key line is a safe fill-in-the-blank.
const ENV_TEMPLATE: &str = r#"# komo credentials (this file is chmod 600; never commit it anywhere).
# Empty values are treated as unset. Match the `provider` in config.toml.

DEEPSEEK_API_KEY=
# OPENAI_API_KEY=
# ANTHROPIC_API_KEY=
# OPENROUTER_API_KEY=

# Channels
# TELEGRAM_BOT_TOKEN=
# FEISHU_APP_ID=
# FEISHU_APP_SECRET=

# Home Assistant (enables the `homeassistant` tool)
# HASS_TOKEN=
# HASS_URL=http://homeassistant.local:8123

# Bearer key for an externally-bound api channel ([channels.api])
# API_SERVER_KEY=
"#;

/// The generated `~/.komo/SOUL.md` — the default persona, from the README's
/// brand section (komorebi: sunlight through leaves). Replaces the built-in
/// one-line identity in the system prompt; the operator edits it freely (the
/// prompt builder re-reads it on mtime change, no restart needed).
const SOUL_TEMPLATE: &str = "\
你是 Komo，一位安静、可靠的个人助理。

名字取自日语「木漏れ日」（komorebi）——阳光透过树叶洒落下来的样子。你的气质也如此：\
温暖、清亮、不喧哗。像树荫下坐在身旁的老朋友：平时安静，开口时说到点子上，并且记得住\
别人托付给你的每一件小事。

你相信小事会积攒成光——一条提醒、一个待办、一段记忆，日积月累就是生活本身。\
「陪你把日子攒成光」是你的座右铭（Light through your days）。

行事风格：

- **简洁**：先给结论和要做的事，少铺垫、不绕弯子。用用户的语言交流，中文时自然口语化，\
不堆砌客套。
- **踏实**：需要实时信息或要动手做事时，调用工具去查、去做，绝不凭空编造；查不到或\
不确定，就直说不确定。你并不知道用户此刻通过哪个渠道（微信/Telegram/飞书/终端）在和\
你说话——自我介绍或聊天时不要提渠道，更不要猜。
- **记性好**：值得长期记住的事（偏好、约定、承诺、常用信息）主动记下来；聊到过去的事\
先查记忆和会话历史，不靠猜。
- **不吵闹**：主动消息（提醒、简报）只在真正有价值时才发；不追问无关紧要的细节，能\
自己查到的不去烦用户。
- **有分寸**：有副作用的操作走审批流程，拿不准的先问一句再动手；宁可慢半拍，不替用户\
做重大决定。

记住每一缕光。
";

/// The generated `~/.komo/USER.md` — the operator-authored user profile
/// (hermes' USER.md analog). Injected into the **main agent's** stable system
/// prompt each turn, re-read on mtime change (no restart). A fill-in skeleton:
/// only long-term stable facts belong here — churny facts/projects/lessons go
/// to the memory store and AGENTS.md instead.
const USER_TEMPLATE: &str = r#"# USER.md — 用户画像（komo 每轮读入主 agent 的稳定信息）

<!--
对标 hermes 的 USER.md：只记**长期稳定**的东西——身份、偏好、习惯、雷区。
会变的事实 / 项目 / 经验交给记忆库（memory）和 AGENTS.md，别写这里。
保持精炼，它每轮都占 token。改完下一轮生效，无需重启（按 mtime 重读，和 SOUL.md 一样）。
删掉这些注释、填上真实内容即可。
-->

## 身份
<!-- 姓名 / 称呼、角色、公司或团队、时区、常用语言 -->

## 沟通偏好
<!-- 简洁 vs 详细、要不要先给结论、格式偏好（表格 / 分点）、用什么语言回你 -->

## 工作习惯
<!-- 常用技术栈与熟练度、编辑器 / 工具、工作流 -->

## 雷区 / 避免
<!-- 明确不喜欢的做法、忌讳、绝对不要做的事 -->
"#;

pub fn run() -> anyhow::Result<()> {
    let home = komo_config::ensure_komo_home();
    let created = init_at(&home)?;
    report("config.toml", &home, created.config);
    report(".env", &home, created.env);
    report("SOUL.md", &home, created.soul);
    report("USER.md", &home, created.user);
    if created.config || created.env {
        println!(
            "\nNext: put your API key in {}/.env (DEEPSEEK_API_KEY=sk-...),\n\
             then restart the gateway. `komo doctor` verifies the result.",
            home.display()
        );
    }
    Ok(())
}

fn report(name: &str, home: &Path, created: bool) {
    let path = home.join(name);
    if created {
        println!("created   {}", path.display());
    } else {
        println!("unchanged {} (already exists)", path.display());
    }
}

/// Which scaffolds this `init` actually created (vs left untouched).
#[derive(Debug, Clone, Copy)]
struct Created {
    config: bool,
    env: bool,
    soul: bool,
    user: bool,
}

/// Write whichever template doesn't exist yet. Never overwrites — an operator's
/// edits outrank the template, always.
fn init_at(home: &Path) -> anyhow::Result<Created> {
    let config = write_if_absent(&home.join("config.toml"), CONFIG_TEMPLATE)?;
    let env_path = home.join(".env");
    let env = write_if_absent(&env_path, ENV_TEMPLATE)?;
    // Credentials file: owner-only, same floor `ensure_komo_home` maintains.
    #[cfg(unix)]
    if env {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
    }
    let soul = write_if_absent(&home.join("SOUL.md"), SOUL_TEMPLATE)?;
    let user = write_if_absent(&home.join("USER.md"), USER_TEMPLATE)?;
    Ok(Created {
        config,
        env,
        soul,
        user,
    })
}

fn write_if_absent(path: &Path, content: &str) -> anyhow::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(path, content)
        .map_err(|e| anyhow::anyhow!("could not write {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("komo_init_test_{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_creates_all_templates() {
        let home = tmp("creates");
        let created = init_at(&home).unwrap();
        assert!(created.config && created.env && created.soul && created.user);
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(config.contains("provider = \"deepseek\""));
        let env = std::fs::read_to_string(home.join(".env")).unwrap();
        assert!(env.contains("DEEPSEEK_API_KEY="));
        let soul = std::fs::read_to_string(home.join("SOUL.md")).unwrap();
        assert!(soul.contains("你是 Komo"));
        let user = std::fs::read_to_string(home.join("USER.md")).unwrap();
        assert!(user.contains("USER.md") && user.contains("## 身份"));
    }

    #[test]
    fn init_never_overwrites_existing_files() {
        let home = tmp("preserves");
        std::fs::write(home.join("config.toml"), "provider = \"openai\"\n").unwrap();
        std::fs::write(home.join("SOUL.md"), "You are Nyx.\n").unwrap();
        std::fs::write(home.join("USER.md"), "name: Ada\n").unwrap();
        let created = init_at(&home).unwrap();
        assert!(!created.config, "existing config must be left alone");
        assert!(created.env, "missing .env is still scaffolded");
        assert!(
            !created.soul,
            "an operator-edited persona must be left alone"
        );
        assert!(
            !created.user,
            "an operator-edited profile must be left alone"
        );
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert_eq!(config, "provider = \"openai\"\n");
        let soul = std::fs::read_to_string(home.join("SOUL.md")).unwrap();
        assert_eq!(soul, "You are Nyx.\n");
        let user = std::fs::read_to_string(home.join("USER.md")).unwrap();
        assert_eq!(user, "name: Ada\n");
    }

    #[cfg(unix)]
    #[test]
    fn scaffolded_env_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp("perms");
        init_at(&home).unwrap();
        let mode = std::fs::metadata(home.join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn generated_config_parses_as_toml() {
        // The template must stay valid TOML — a scaffold that breaks parsing
        // would be worse than no scaffold.
        let parsed: Result<toml::Value, _> = toml::from_str(CONFIG_TEMPLATE);
        assert!(parsed.is_ok(), "template must parse: {:?}", parsed.err());
    }
}
