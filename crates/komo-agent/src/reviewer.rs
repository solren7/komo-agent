use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use komo_core::domain::{
    llm::LlmClient,
    memory::{
        Memory, MemoryConfidence, MemoryContext, MemoryKind, MemoryRepository, MemoryStatus,
        parse_memory_kind,
    },
    message::{Message, Role},
    repository::SkillRepository,
    reviewer::{ReviewOutcome, Reviewer, SELF_REVIEW_PROMPT},
    session::Session,
    skill::{SOURCE_REVIEWER, Skill},
    task::{Task, TaskRepository, TaskStatus},
};

pub struct ReflectiveReviewer {
    llm: Arc<dyn LlmClient>,
    memories: Arc<dyn MemoryRepository>,
    skills: Arc<dyn SkillRepository>,
    tasks: Arc<dyn TaskRepository>,
}

impl ReflectiveReviewer {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        memories: Arc<dyn MemoryRepository>,
        skills: Arc<dyn SkillRepository>,
        tasks: Arc<dyn TaskRepository>,
    ) -> Self {
        Self {
            llm,
            memories,
            skills,
            tasks,
        }
    }

    /// A synthetic single-message session for an aux call.
    ///
    /// `model`/`effort` stay empty on purpose: that is what keeps the reviewed
    /// conversation's model choice from leaking onto the aux model (the same
    /// invariant every other aux path holds).
    fn aux_session(&self, session: &Session, prompt: String) -> Session {
        Session {
            id: format!("review-{}", session.id),
            workspace: session.workspace.clone(),
            messages: vec![Message::user(prompt)],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            title: String::new(),
            status: String::new(),
            model: String::new(),
            effort: String::new(),
        }
    }

    /// Fold a proposed change into a skill's **real** body, and return the
    /// complete replacement — or `None`, which the caller treats as "drop this
    /// proposal".
    ///
    /// This is the read half of read-before-write. The reviewer's transcript is
    /// user and assistant text only — tool results are never persisted as
    /// messages — so it has never seen a single skill body, yet `instructions`
    /// is a *whole* body and `promote` writes it over the active file. Without
    /// this pass, every update proposal is a rewrite-from-imagination of a file
    /// the writer never opened.
    ///
    /// A failed or empty second pass returns `None` rather than falling back to
    /// the ungrounded text: dropping a proposal costs one missed improvement,
    /// writing it costs the operator's skill.
    async fn grounded_rewrite(
        &self,
        session: &Session,
        current: &Skill,
        proposed: &str,
    ) -> Option<String> {
        let prompt = rewrite_prompt(current, proposed);
        let reply = match self.llm.complete(&self.aux_session(session, prompt)).await {
            Ok(reply) => reply,
            Err(error) => {
                tracing::warn!(%error, name = %current.name, "grounded skill rewrite failed");
                return None;
            }
        };
        let body = strip_code_fence(&reply).trim().to_string();
        (!body.is_empty()).then_some(body)
    }
}

#[async_trait]
impl Reviewer for ReflectiveReviewer {
    async fn review(&self, session: &Session) -> anyhow::Result<ReviewOutcome> {
        // The catalog is what lets the model target an *existing* skill by its
        // real name instead of inventing a near-duplicate. It has never been in
        // this prompt, which is why the skill ladder's "patch an existing
        // skill" steps had nothing to aim at.
        let catalog = skill_catalog(&self.skills.list().await.unwrap_or_default());
        let prompt = review_prompt(session, &catalog);
        let reply = self
            .llm
            .complete(&self.aux_session(session, prompt))
            .await?;
        let Some(suggestions) = parse_suggestions(&reply)? else {
            return Ok(ReviewOutcome::default());
        };

        let ctx = MemoryContext::from_session(&session.id);
        let mut outcome = ReviewOutcome::default();

        // Load the memory store once and derive both dedup guards from it,
        // instead of re-scanning per suggestion (the sweep runs the reviewer
        // over every active session — the per-suggestion scans were O(sessions ×
        // suggestions) full-table reads).
        //
        //  - `known_keys` (anti-self-cannibalization, roadmap §5): content keys
        //    of active, in-scope memories — exactly the set eligible to be
        //    recalled into this session. A re-extraction matching one is the
        //    assistant echoing its own injected context, not a fresh user
        //    disclosure. Exact content-key match only; fuzzy/semantic dedup is
        //    deferred to embedding-based recall (§5-D).
        //  - `seen_keys` (cross-sweep dedup): the `source_message_id`s this
        //    session already produced (any status), mirroring
        //    `find_by_source_message_id(session.id, key)`. Mutable so a duplicate
        //    within this same review is also caught.
        let all_memories = self.memories.list().await?;
        let known_keys: std::collections::HashSet<String> = all_memories
            .iter()
            .filter(|m| m.status == MemoryStatus::Active && ctx.allows(&m.scope))
            .map(|m| memory_key(&m.content))
            .collect();
        let mut seen_keys: std::collections::HashSet<String> = all_memories
            .iter()
            .filter(|m| m.source == session.id && !m.source_message_id.is_empty())
            .map(|m| m.source_message_id.clone())
            .collect();

        for suggestion in suggestions.memories {
            if should_skip(&suggestion.content) {
                continue;
            }
            let kind = suggestion
                .kind
                .as_deref()
                .map(parse_memory_kind)
                .unwrap_or(MemoryKind::Fact);
            // Content-derived dedup key: skip if this session already produced
            // the same fact (an earlier sweep or earlier in this review), or if
            // komo already holds it as an active, in-scope memory.
            let key = memory_key(&suggestion.content);
            if seen_keys.contains(&key) || known_keys.contains(&key) {
                continue;
            }
            let mut memory = Memory::new(kind, suggestion.content);
            // Automated extraction is a low-trust suggestion: it lands as a
            // Candidate the user confirms or discards (same governance as task
            // inbox), never a pinned/active memory. Scope it to the origin so a
            // channel-scoped fact never leaks into another chat.
            memory.status = MemoryStatus::Candidate;
            memory.confidence = MemoryConfidence::Extracted;
            memory.scope = ctx.write_scope();
            // Tag the origin so a later answer can trace why komo believes this.
            memory.source = session.id.clone();
            memory.source_message_id = key.clone();
            self.memories.save(&memory).await?;
            seen_keys.insert(key);
            outcome.memories_written.push(memory.id);
        }

        for suggestion in suggestions.skills {
            if should_skip(&suggestion.instructions) {
                continue;
            }
            let existing = self.skills.find(&suggestion.name).await?;
            // Protected = operator edits only: no candidate proposal either,
            // so a "just promote it" nudge can never overwrite the operator's
            // version (roadmap §9 — protection guards proposal *generation*).
            if existing.as_ref().is_some_and(|s| s.protected) {
                continue;
            }
            // Read-before-write: a proposal that replaces an existing skill is
            // re-derived from that skill's real body first. New skills have
            // nothing to read, so they go through as written.
            let instructions = match &existing {
                Some(current) => {
                    match self
                        .grounded_rewrite(session, current, &suggestion.instructions)
                        .await
                    {
                        Some(body) => body,
                        None => {
                            tracing::info!(
                                name = %suggestion.name,
                                "skill update proposal dropped — could not ground it in the current body"
                            );
                            continue;
                        }
                    }
                }
                None => suggestion.instructions,
            };
            let skill = Skill {
                name: suggestion.name,
                description: suggestion
                    .description
                    .or_else(|| existing.as_ref().map(|s| s.description.clone()))
                    .unwrap_or_default(),
                instructions,
                protected: false,
                disabled: false,
                source: SOURCE_REVIEWER.to_string(),
                // Offer gating is the operator's, not the reviewer's: an update
                // proposal inherits it so promoting one can't quietly widen
                // where a platform- or tool-gated skill gets advertised.
                platforms: existing
                    .as_ref()
                    .map(|s| s.platforms.clone())
                    .unwrap_or_default(),
                requires_tools: existing
                    .as_ref()
                    .map(|s| s.requires_tools.clone())
                    .unwrap_or_default(),
            };
            // `save` writes a *candidate* (never an active skill) — automated
            // extraction goes through triage like memory candidates. A refused
            // proposal (bad name, protected race) must not fail the review.
            if let Err(error) = self.skills.save(&skill).await {
                tracing::warn!(%error, name = %skill.name, "skill proposal not written");
                continue;
            }
            outcome.skills_written.push(skill.name);
        }

        // Commitments land in the inbox only, never straight to `todo`: automated
        // extraction is a suggestion the user confirms or discards (same governance
        // as memory writes). `source_message_id` is a content-derived dedup key so
        // re-reviewing the same session across sweeps never duplicates a task.
        for commitment in suggestions.commitments {
            let title = commitment.title.trim();
            if title.is_empty() || should_skip(title) {
                continue;
            }
            let key = commitment_key(title);
            if self
                .tasks
                .find_by_source_message_id(&session.id, &key)
                .await?
                .is_some()
            {
                continue;
            }
            let mut task = Task::new(title.to_string());
            task.status = TaskStatus::Inbox;
            task.note = commitment.note.unwrap_or_default();
            task.waiting_on = commitment.waiting_on.unwrap_or_default();
            task.source = session.id.clone();
            task.source_message_id = key;
            self.tasks.save(&task).await?;
            outcome.tasks_captured.push(task.id);
        }

        Ok(outcome)
    }
}

/// Deterministic, dependency-free dedup key for an extracted commitment: FNV-1a
/// over the whitespace-normalized lowercased title. Stable across sweeps and
/// platforms, so the same obligation always maps to the same key.
fn commitment_key(title: &str) -> String {
    format!("commit-{:016x}", fnv1a(title))
}

/// Content-derived dedup key for an extracted memory (same FNV-1a-over-
/// normalized-text scheme as [`commitment_key`]), so a re-extraction of the
/// same fact maps to the same key.
fn memory_key(content: &str) -> String {
    format!("mem-{:016x}", fnv1a(content))
}

/// FNV-1a over whitespace-normalized lowercased text. Deterministic, dependency-
/// free, stable across sweeps and platforms.
fn fnv1a(text: &str) -> u64 {
    let norm = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in norm.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `- name: description` lines for the active skills, or empty when there are
/// none. Descriptions only — bodies are what the second pass is for.
fn skill_catalog(skills: &[Skill]) -> String {
    skills
        .iter()
        .filter(|skill| !skill.disabled)
        .map(|skill| format!("- {}: {}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The second pass: the model is shown the skill it asked to change, and
/// returns the whole updated body. Deliberately prose-in/prose-out — there is
/// one value to produce, so wrapping it in JSON would only add a parse that can
/// fail.
fn rewrite_prompt(current: &Skill, proposed: &str) -> String {
    format!(
        "You proposed a change to the existing skill `{}`. Below is its CURRENT body \
         — the text you are replacing — followed by what you proposed.\n\n\
         Return ONLY the complete updated skill body: the full replacement text, no \
         diff, no summary, no commentary, no frontmatter. Keep everything in the \
         current body that your change does not specifically improve; you are editing \
         a file someone else wrote, not rewriting it from scratch. If the current body \
         already covers your change, return it unchanged.\n\n\
         CURRENT body of `{}`:\n---\n{}\n---\n\n\
         Proposed change:\n---\n{proposed}\n---",
        current.name, current.name, current.instructions
    )
}

fn review_prompt(session: &Session, catalog: &str) -> String {
    let transcript = session
        .messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n\n");
    // Naming an existing skill means "change this one". Say so explicitly, and
    // say that the body is not here — otherwise the model writes a replacement
    // body for a file it has never read, which is exactly what the second pass
    // exists to prevent.
    let existing = if catalog.is_empty() {
        "There are no existing skills yet; any skill you return is a new one.\n\n".to_string()
    } else {
        format!(
            "Existing skills (name: description) — return one of these exact names to \
             change it, or a new name to create one. You are NOT shown their bodies: \
             when changing one, write `instructions` as the change you want made, and \
             it will be folded into the real body for you.\n{catalog}\n\n"
        )
    };

    format!(
        "{SELF_REVIEW_PROMPT}\n\n{existing}Return only JSON in this exact shape:\n\
         {{\"memories\":[{{\"kind\":\"profile|preference|feedback|project|person|fact|decision|reference\",\"content\":\"...\"}}],\
         \"skills\":[{{\"name\":\"class-level-skill-name\",\"description\":\"...\",\
         \"instructions\":\"full patched skill body\"}}],\
         \"commitments\":[{{\"title\":\"short actionable obligation\",\
         \"waiting_on\":\"who it involves, or empty\",\"note\":\"context/deadline, or empty\"}}]}}\n\
         Use empty arrays when nothing durable should be written.\n\n\
         Session transcript:\n{transcript}"
    )
}

fn render_message(message: &Message) -> String {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    format!("{role}: {}", message.content)
}

#[derive(Debug, Deserialize)]
struct ReviewSuggestions {
    #[serde(default)]
    memories: Vec<MemorySuggestion>,
    #[serde(default)]
    skills: Vec<SkillSuggestion>,
    #[serde(default)]
    commitments: Vec<CommitmentSuggestion>,
}

#[derive(Debug, Deserialize)]
struct CommitmentSuggestion {
    title: String,
    #[serde(default)]
    waiting_on: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MemorySuggestion {
    /// A free-form kind string parsed leniently (`parse_memory_kind` accepts the
    /// legacy `user` vocabulary and falls back to `fact`), so a model returning
    /// an out-of-vocabulary kind never fails the whole extraction.
    #[serde(default)]
    kind: Option<String>,
    content: String,
}

#[derive(Debug, Deserialize)]
struct SkillSuggestion {
    name: String,
    #[serde(default)]
    description: Option<String>,
    instructions: String,
}

fn parse_suggestions(reply: &str) -> anyhow::Result<Option<ReviewSuggestions>> {
    let json = extract_json(reply).trim();
    if json.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(json)?))
}

/// The second pass returns prose, not JSON, but the fence handling is the same:
/// models wrap a returned document in a code fence about half the time.
fn strip_code_fence(reply: &str) -> &str {
    extract_json(reply)
}

fn extract_json(reply: &str) -> &str {
    if let Some(start) = reply.find("```json") {
        let after_fence = &reply[start + "```json".len()..];
        if let Some(end) = after_fence.find("```") {
            return &after_fence[..end];
        }
    }
    if let Some(start) = reply.find("```") {
        let after_fence = &reply[start + "```".len()..];
        if let Some(end) = after_fence.find("```") {
            return &after_fence[..end];
        }
    }
    reply
}

fn should_skip(content: &str) -> bool {
    let text = content.to_lowercase();
    [
        "command not found",
        "missing credential",
        "missing credentials",
        "package not installed",
        "tool is broken",
        "tool broke",
        "retry fixed",
        "transient",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::skill::Skill;
    use std::sync::Mutex;

    // ── fakes ─────────────────────────────────────────────────────────────────

    struct FixedLlm(String);

    #[async_trait]
    impl LlmClient for FixedLlm {
        async fn complete(&self, _session: &Session) -> anyhow::Result<String> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct FakeMemories(Mutex<Vec<Memory>>);

    #[async_trait]
    impl MemoryRepository for FakeMemories {
        async fn list(&self) -> anyhow::Result<Vec<Memory>> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn save(&self, memory: &Memory) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(memory.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeSkills(Mutex<Vec<Skill>>);

    #[async_trait]
    impl SkillRepository for FakeSkills {
        async fn find(&self, name: &str) -> anyhow::Result<Option<Skill>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.name == name)
                .cloned())
        }
        async fn list(&self) -> anyhow::Result<Vec<Skill>> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn save(&self, skill: &Skill) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(skill.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeTasks(Mutex<Vec<Task>>);

    #[async_trait]
    impl TaskRepository for FakeTasks {
        async fn save(&self, task: &Task) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(task.clone());
            Ok(())
        }
        async fn find(&self, id: &str) -> anyhow::Result<Option<Task>> {
            Ok(self.0.lock().unwrap().iter().find(|t| t.id == id).cloned())
        }
        async fn list_open(&self) -> anyhow::Result<Vec<Task>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.status.is_open())
                .cloned()
                .collect())
        }
        async fn update(&self, task: &Task) -> anyhow::Result<()> {
            let mut rows = self.0.lock().unwrap();
            if let Some(slot) = rows.iter_mut().find(|t| t.id == task.id) {
                *slot = task.clone();
            }
            Ok(())
        }
        async fn find_by_source_message_id(
            &self,
            source: &str,
            source_message_id: &str,
        ) -> anyhow::Result<Option<Task>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.source == source && t.source_message_id == source_message_id)
                .cloned())
        }
    }

    fn reviewer_with(reply: &str) -> (ReflectiveReviewer, Arc<FakeTasks>) {
        let tasks = Arc::new(FakeTasks::default());
        let reviewer = ReflectiveReviewer::new(
            Arc::new(FixedLlm(reply.to_string())),
            Arc::new(FakeMemories::default()),
            Arc::new(FakeSkills::default()),
            tasks.clone(),
        );
        (reviewer, tasks)
    }

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            workspace: "__default__".to_string(),
            messages: vec![Message::user(
                "I'll send Bob the report tomorrow".to_string(),
            )],
            created_at: 0,
            title: String::new(),
            status: String::new(),
            model: String::new(),
            effort: String::new(),
        }
    }

    // ── skill extraction ───────────────────────────────────────────────────────

    /// Replies handed out in order, with every prompt it was asked recorded —
    /// the two-pass skill path is only correct if the *second* prompt carries
    /// the current body, so the test has to see the prompts.
    #[derive(Default)]
    struct ScriptedLlm {
        replies: Mutex<std::collections::VecDeque<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl ScriptedLlm {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().map(|r| r.to_string()).collect()),
                prompts: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedLlm {
        async fn complete(&self, session: &Session) -> anyhow::Result<String> {
            self.prompts
                .lock()
                .unwrap()
                .push(session.messages[0].content.clone());
            Ok(self.replies.lock().unwrap().pop_front().unwrap_or_default())
        }
    }

    fn active_skill(name: &str, body: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("does {name}"),
            instructions: body.to_string(),
            protected: false,
            disabled: false,
            source: "user".to_string(),
            platforms: Vec::new(),
            requires_tools: Vec::new(),
        }
    }

    fn skill_reviewer(
        llm: Arc<ScriptedLlm>,
        existing: Vec<Skill>,
    ) -> (ReflectiveReviewer, Arc<FakeSkills>) {
        let skills = Arc::new(FakeSkills(Mutex::new(existing)));
        let reviewer = ReflectiveReviewer::new(
            llm,
            Arc::new(FakeMemories::default()),
            skills.clone(),
            Arc::new(FakeTasks::default()),
        );
        (reviewer, skills)
    }

    const PATCH_DEPLOY: &str = r#"{"memories":[],"commitments":[],"skills":[{"name":"deploy","description":"ship it","instructions":"also run the tests first"}]}"#;

    /// The hole this closes: the reviewer never sees a skill body (tool results
    /// are not persisted as messages), so an update proposal is written blind
    /// and `promote` writes it over the real file. The second pass is the read.
    #[tokio::test]
    async fn an_update_proposal_is_rewritten_against_the_real_body() {
        let llm = ScriptedLlm::new(&[PATCH_DEPLOY, "STEP ONE\nSTEP TWO\nSTEP THREE: run tests"]);
        let (reviewer, skills) = skill_reviewer(
            llm.clone(),
            vec![active_skill("deploy", "STEP ONE\nSTEP TWO")],
        );

        let outcome = reviewer.review(&session("api:1")).await.unwrap();
        assert_eq!(outcome.skills_written, ["deploy"]);

        let prompts = llm.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2, "an update takes a grounding pass");
        // Pass 1 knows the skill exists, but not what it says.
        assert!(prompts[0].contains("deploy: does deploy"));
        assert!(!prompts[0].contains("STEP TWO"));
        // Pass 2 is handed the actual file plus the requested change.
        assert!(prompts[1].contains("STEP ONE\nSTEP TWO"));
        assert!(prompts[1].contains("also run the tests first"));

        // What gets written is the grounded body, never the blind proposal.
        let written = skills.0.lock().unwrap();
        let candidate = written
            .iter()
            .find(|s| s.source == SOURCE_REVIEWER)
            .unwrap();
        assert!(candidate.instructions.contains("STEP TWO"));
        assert!(candidate.instructions.contains("run tests"));
        assert!(!candidate.instructions.contains("also run the tests first"));
    }

    /// Failing to ground drops the proposal — it must never fall back to the
    /// blind body, which is the exact thing being guarded against.
    #[tokio::test]
    async fn an_ungroundable_update_is_dropped_not_written() {
        // Second pass yields nothing usable.
        let llm = ScriptedLlm::new(&[PATCH_DEPLOY, "   "]);
        let (reviewer, skills) =
            skill_reviewer(llm, vec![active_skill("deploy", "STEP ONE\nSTEP TWO")]);

        let outcome = reviewer.review(&session("api:1")).await.unwrap();
        assert!(outcome.skills_written.is_empty());
        let written = skills.0.lock().unwrap();
        assert!(written.iter().all(|s| s.source != SOURCE_REVIEWER));
    }

    /// A brand-new skill has no body to read, so it costs no second call and
    /// goes through exactly as proposed.
    #[tokio::test]
    async fn a_new_skill_needs_no_grounding_pass() {
        let llm = ScriptedLlm::new(&[PATCH_DEPLOY]);
        let (reviewer, skills) = skill_reviewer(llm.clone(), Vec::new());

        let outcome = reviewer.review(&session("api:1")).await.unwrap();
        assert_eq!(outcome.skills_written, ["deploy"]);
        assert_eq!(llm.prompts.lock().unwrap().len(), 1);
        assert_eq!(
            skills.0.lock().unwrap()[0].instructions,
            "also run the tests first"
        );
    }

    /// Protection still wins before any of this: no grounding call, no proposal.
    #[tokio::test]
    async fn a_protected_skill_is_never_even_grounded() {
        let mut protected = active_skill("deploy", "STEP ONE");
        protected.protected = true;
        let llm = ScriptedLlm::new(&[PATCH_DEPLOY, "rewritten"]);
        let (reviewer, skills) = skill_reviewer(llm.clone(), vec![protected]);

        let outcome = reviewer.review(&session("api:1")).await.unwrap();
        assert!(outcome.skills_written.is_empty());
        assert_eq!(llm.prompts.lock().unwrap().len(), 1);
        assert!(
            skills
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|s| s.source != SOURCE_REVIEWER)
        );
    }

    // ── commitment extraction ──────────────────────────────────────────────────

    #[tokio::test]
    async fn captures_commitment_into_inbox() {
        let reply = r#"{"memories":[],"skills":[],"commitments":[{"title":"send Bob the report","waiting_on":"Bob","note":"by tomorrow"}]}"#;
        let (reviewer, tasks) = reviewer_with(reply);

        let outcome = reviewer.review(&session("telegram:42")).await.unwrap();
        assert_eq!(outcome.tasks_captured.len(), 1);

        let rows = tasks.0.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, TaskStatus::Inbox);
        assert_eq!(rows[0].waiting_on, "Bob");
        assert_eq!(rows[0].source, "telegram:42");
        assert!(!rows[0].source_message_id.is_empty());
    }

    #[tokio::test]
    async fn dedups_commitment_across_repeated_reviews() {
        let reply = r#"{"commitments":[{"title":"send Bob the report"}]}"#;
        let (reviewer, tasks) = reviewer_with(reply);
        let s = session("telegram:42");

        reviewer.review(&s).await.unwrap();
        let second = reviewer.review(&s).await.unwrap();

        // Same session + same commitment → no duplicate on the second sweep.
        assert_eq!(second.tasks_captured.len(), 0);
        assert_eq!(tasks.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn same_commitment_in_different_sessions_is_not_deduped() {
        let reply = r#"{"commitments":[{"title":"send Bob the report"}]}"#;
        let (reviewer, tasks) = reviewer_with(reply);

        reviewer.review(&session("telegram:1")).await.unwrap();
        reviewer.review(&session("telegram:2")).await.unwrap();

        assert_eq!(tasks.0.lock().unwrap().len(), 2);
    }

    #[test]
    fn commitment_key_is_stable_under_whitespace_and_case() {
        assert_eq!(
            commitment_key("Send  Bob the REPORT"),
            commitment_key("send bob the report")
        );
    }

    #[test]
    fn extracts_fenced_json() {
        let parsed = parse_suggestions(
            "```json\n{\"memories\":[{\"kind\":\"user\",\"content\":\"prefers concise replies\"}],\"skills\":[]}\n```",
        )
        .unwrap()
        .unwrap();

        assert_eq!(parsed.memories.len(), 1);
        // Legacy `user` kind parses leniently to `Profile`.
        assert_eq!(
            parsed.memories[0].kind.as_deref().map(parse_memory_kind),
            Some(MemoryKind::Profile)
        );
    }

    #[tokio::test]
    async fn extracted_memory_lands_as_scoped_candidate() {
        let reply = r#"{"memories":[{"kind":"preference","content":"prefers concise replies"}],"skills":[],"commitments":[]}"#;
        let tasks = Arc::new(FakeTasks::default());
        let memories = Arc::new(FakeMemories::default());
        let reviewer = ReflectiveReviewer::new(
            Arc::new(FixedLlm(reply.to_string())),
            memories.clone(),
            Arc::new(FakeSkills::default()),
            tasks,
        );

        reviewer.review(&session("telegram:42")).await.unwrap();

        let rows = memories.0.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, MemoryStatus::Candidate);
        assert_eq!(rows[0].confidence, MemoryConfidence::Extracted);
        assert_eq!(
            rows[0].scope,
            komo_core::domain::memory::MemoryScope::Channel {
                platform: "telegram".into(),
                chat_id: "42".into()
            }
        );
        assert!(!rows[0].source_message_id.is_empty());
    }

    #[tokio::test]
    async fn dedups_extracted_memory_across_repeated_reviews() {
        let reply = r#"{"memories":[{"kind":"fact","content":"komo uses Rust"}],"skills":[],"commitments":[]}"#;
        let memories = Arc::new(FakeMemories::default());
        let reviewer = ReflectiveReviewer::new(
            Arc::new(FixedLlm(reply.to_string())),
            memories.clone(),
            Arc::new(FakeSkills::default()),
            Arc::new(FakeTasks::default()),
        );
        let s = session("telegram:42");

        reviewer.review(&s).await.unwrap();
        reviewer.review(&s).await.unwrap();

        // Same session + same fact → no duplicate on the second sweep.
        assert_eq!(memories.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn does_not_re_extract_a_known_active_memory() {
        // komo already holds this fact as an active, in-scope memory — distilled
        // from a *different* session, so the per-session source dedup can't catch
        // it. The reviewer must still refuse to re-ingest it (the assistant likely
        // echoed a recalled fact), instead of minting a duplicate candidate.
        let reply = r#"{"memories":[{"kind":"fact","content":"komo uses Rust"}],"skills":[],"commitments":[]}"#;
        let memories = Arc::new(FakeMemories::default());
        let mut existing = Memory::new(MemoryKind::Fact, "komo uses Rust");
        existing.status = MemoryStatus::Active;
        existing.scope = komo_core::domain::memory::MemoryScope::Channel {
            platform: "telegram".into(),
            chat_id: "42".into(),
        };
        existing.source = "telegram:99".into(); // a different origin session
        memories.save(&existing).await.unwrap();

        let reviewer = ReflectiveReviewer::new(
            Arc::new(FixedLlm(reply.to_string())),
            memories.clone(),
            Arc::new(FakeSkills::default()),
            Arc::new(FakeTasks::default()),
        );
        let outcome = reviewer.review(&session("telegram:42")).await.unwrap();

        assert!(outcome.memories_written.is_empty());
        // Only the pre-existing memory remains; no duplicate candidate added.
        assert_eq!(memories.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn re_extracts_known_memory_from_another_scope() {
        // The same fact held active but scoped to a *different* channel was never
        // eligible to be recalled into this session, so it is not self-echo — a
        // channel-scoped candidate is still captured here.
        let reply = r#"{"memories":[{"kind":"fact","content":"komo uses Rust"}],"skills":[],"commitments":[]}"#;
        let memories = Arc::new(FakeMemories::default());
        let mut existing = Memory::new(MemoryKind::Fact, "komo uses Rust");
        existing.status = MemoryStatus::Active;
        existing.scope = komo_core::domain::memory::MemoryScope::Channel {
            platform: "feishu".into(),
            chat_id: "oc_x".into(),
        };
        memories.save(&existing).await.unwrap();

        let reviewer = ReflectiveReviewer::new(
            Arc::new(FixedLlm(reply.to_string())),
            memories.clone(),
            Arc::new(FakeSkills::default()),
            Arc::new(FakeTasks::default()),
        );
        reviewer.review(&session("telegram:42")).await.unwrap();

        assert_eq!(memories.0.lock().unwrap().len(), 2);
    }

    #[test]
    fn skips_environment_failures() {
        assert!(should_skip("npm failed with command not found"));
        assert!(!should_skip("User asked for concise status updates"));
    }
}
