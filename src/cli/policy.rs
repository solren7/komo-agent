//! `komo policy` — inspect and dry-run the permission policy (roadmap §3).
//!
//! `list` shows the resolved rules exactly as the `PolicyApprover` will apply
//! them (invalid config entries are already filtered out, and reported); the
//! rule numbers here are the ones `check` cites. `check` dry-runs one action
//! through `Policy::decide` with the same risk classification the real tools
//! use, so what it prints is what a turn would do. Pure config parsing — no
//! db, no gateway.

use std::path::PathBuf;

use crate::domain::approval::{ActionRef, ApprovalRequest, Risk};
use crate::domain::policy::{Category, Policy, Rule, RuleSource, Verdict};
use komo_config::{ConfigSnapshot, PolicyReport};
use komo_infra::permissions_store::PermissionsStore;

/// Rendering lives on the rule itself, so `policy list`, `saved list`, and the
/// approval prompt can't describe the same rule three different ways.
fn describe_rule(r: &Rule) -> String {
    r.describe()
}

/// Render the resolved policy: defaults, rules in evaluation order, and any
/// config entries that failed to parse.
pub fn list(config: &ConfigSnapshot) -> anyhow::Result<()> {
    let PolicyReport {
        policy,
        invalid,
        configured,
    } = &config.runtime.policy;

    let saved = PermissionsStore::load(&config.runtime.home);
    if !configured && saved.is_empty() {
        println!(
            "No [policy] table in {} — every Normal/Dangerous action asks interactively.",
            config.runtime.home.join("config.toml").display()
        );
        return Ok(());
    }

    println!("default_normal: {}", verdict_str(policy.default_normal()));
    println!("(Dangerous always asks unless a rule sets include_dangerous; Safe is deny-only)");

    if policy.rules().is_empty() {
        println!("\nno rules configured");
    } else {
        println!("\nrules (deny rules always win over allow):");
        for (i, r) in policy.rules().iter().enumerate() {
            println!("  #{i} {}", describe_rule(r));
        }
    }

    // Saved grants are listed apart from config rules, and after them, because
    // that is the order they are evaluated in — a config deny still wins.
    println!();
    print_saved(&saved);

    if !invalid.is_empty() {
        println!(
            "\n✗ {} invalid [[policy.rule]] entr{} ignored (config order: {})",
            invalid.len(),
            if invalid.len() == 1 { "y" } else { "ies" },
            invalid
                .iter()
                .map(|i| format!("#{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Dry-run one action through the policy and explain the outcome.
pub fn check(
    config: &ConfigSnapshot,
    category: &str,
    target: &str,
    channel: Option<&str>,
    dangerous: bool,
    write: bool,
) -> anyhow::Result<()> {
    let Some(cat) = Category::parse(category) else {
        anyhow::bail!(
            "unknown category `{category}` (expected shell | file | network | homeassistant | mcp)"
        );
    };

    // Mirror the risk each real tool would attach, so the dry run matches a turn.
    let (action, risk) = match cat {
        Category::Shell => (
            ActionRef::Shell {
                command: target.to_string(),
            },
            if dangerous {
                Risk::Dangerous
            } else {
                Risk::Normal
            },
        ),
        Category::File => (
            ActionRef::File {
                path: PathBuf::from(target),
                write,
            },
            if write { Risk::Normal } else { Risk::Safe },
        ),
        Category::Network => (
            ActionRef::Network {
                url: target.to_string(),
            },
            Risk::Safe,
        ),
        Category::HomeAssistant => {
            let Some((domain, service)) = target.split_once('.') else {
                anyhow::bail!("homeassistant target must be `domain.service` (e.g. light.turn_on)");
            };
            (
                ActionRef::Service {
                    domain: domain.to_string(),
                    service: service.to_string(),
                },
                Risk::Normal,
            )
        }
        Category::Mcp => {
            let Some((server, tool)) = target.split_once('.') else {
                anyhow::bail!("mcp target must be `server.tool` (e.g. memos.create_memo)");
            };
            (
                ActionRef::Mcp {
                    server: server.to_string(),
                    tool: tool.to_string(),
                },
                // Every MCP tool asks: a remote can't declare itself read-only.
                Risk::Normal,
            )
        }
    };

    let mut request = ApprovalRequest::normal(format!("check {category} {target}"));
    request.risk = risk;
    let request = request.with_action(action);

    // Evaluate exactly what a turn would: config rules plus the operator's saved
    // grants. (Saved grants are skipped for a dangerous action and for a
    // channel-less/unattended check — the engine, not this command, decides that.)
    let store = PermissionsStore::load(&config.runtime.home);
    let policy: Policy = config
        .runtime
        .policy
        .policy
        .clone()
        .with_saved(store.rules());
    let decision = policy.decide(&request, channel);
    let matched = |i: usize| match decision.source {
        RuleSource::Saved => format!("saved #{i} {}", describe_rule(&policy.saved_rules()[i])),
        // `check` never evaluates a job's grants — it dry-runs config + saved.
        RuleSource::JobGrant => format!("job grant #{i}"),
        RuleSource::Config => format!("#{i} {}", describe_rule(&policy.rules()[i])),
    };

    let risk_str = match risk {
        Risk::Safe => "safe (read-only)",
        Risk::Normal => "normal",
        Risk::Dangerous => "dangerous",
    };
    println!(
        "action:  {category} {target}  [risk: {risk_str}{}]",
        channel
            .map(|c| format!(", channel: {c}"))
            .unwrap_or_else(|| ", no session (unattended context)".to_string())
    );

    match (decision.verdict, decision.rule) {
        (Verdict::Deny, Some(i)) => {
            println!("verdict: DENY — hard-blocked, no prompt");
            println!("matched: {}", matched(i));
        }
        (Verdict::Allow, Some(i)) => {
            println!("verdict: ALLOW — auto-allowed inside a session turn (no prompt)");
            println!("matched: {}", matched(i));
            if decision.source == RuleSource::Saved {
                println!(
                    "note:    a saved grant (`komo policy saved list`); forget it to be asked again"
                );
            }
            println!(
                "note:    with no session in scope (sweep/aux), this still falls to ask → deny"
            );
        }
        (Verdict::Allow, None) if risk == Risk::Safe => {
            println!(
                "verdict: ALLOW — read-only action, no deny rule matches (deny-only evaluation)"
            );
            println!(
                "note:    allow rules never apply to safe actions; only a deny rule can block this"
            );
        }
        (Verdict::Allow, None) => {
            println!("verdict: ALLOW — default_normal = allow (no rule matched)");
        }
        (Verdict::Ask, _) => {
            println!(
                "verdict: ASK — escalates to interactive approval (/approve in chat, y/N at the CLI)"
            );
            if risk == Risk::Dangerous {
                println!(
                    "note:    dangerous actions auto-allow only via a rule with include_dangerous = true"
                );
            }
        }
        (Verdict::Deny, None) => {
            println!("verdict: DENY — default_normal = deny (no rule matched)");
        }
    }
    Ok(())
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Deny => "deny",
        Verdict::Ask => "ask",
    }
}

/// `komo policy saved list` — the grants accumulated by answering `a` at an
/// approval prompt, numbered as `forget` takes them.
pub fn saved_list(config: &ConfigSnapshot) -> anyhow::Result<()> {
    let store = PermissionsStore::load(&config.runtime.home);
    println!("{}", store.path().display());
    print_saved(&store);
    Ok(())
}

/// `komo policy saved forget <n>` / `--all` — stop honoring a grant, so the next
/// matching action asks again.
pub fn saved_forget(
    config: &ConfigSnapshot,
    index: Option<usize>,
    all: bool,
) -> anyhow::Result<()> {
    let store = PermissionsStore::load(&config.runtime.home);
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    match (index, all) {
        (_, true) => {
            let removed = store.forget_all(&now);
            println!("Forgot {removed} saved grant(s); those actions will ask again.");
        }
        (Some(i), false) => match store.forget(i, &now) {
            Some(rule) => println!("Forgot #{i} {} — it will ask again.", rule.describe()),
            None => anyhow::bail!(
                "no saved grant #{i} (there {} {})",
                if store.len() == 1 { "is" } else { "are" },
                match store.len() {
                    0 => "none".to_string(),
                    n => format!("{n}"),
                }
            ),
        },
        (None, false) => anyhow::bail!("pass an index (see `komo policy saved list`) or --all"),
    }
    Ok(())
}

fn print_saved(store: &PermissionsStore) {
    let rules = store.list();
    if rules.is_empty() {
        println!("saved grants: none (answer `a` at an approval prompt to add one)");
        return;
    }
    println!(
        "saved grants ({}, from approval prompts — evaluated after config rules, \
         never for dangerous or unattended actions):",
        rules.len()
    );
    for (i, r) in rules.iter().enumerate() {
        println!("  #{i} {}", describe_rule(r));
    }
}
