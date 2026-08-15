//! Transcripts as append-only JSONL — one file per session under
//! `~/.komo/sessions/`.
//!
//! ## Why this is not a table
//!
//! A transcript is the one thing in komo that is purely *appended*: a turn adds
//! a user message and an assistant message, and nothing ever revisits them. That
//! shape costs nothing in a table, but the schema does — a table's shape is
//! fixed at creation, and toasty's `push_schema` runs only for a new database
//! file, so a non-additive change to the message shape means deleting
//! `state.db`. A line of JSON has no shape to migrate: a new field reads as its
//! default on every line written before it existed, and a change too deep for
//! that is dispatched on [`Line::v`] rather than paid for with the file.
//!
//! It also drops a piece of bookkeeping the table needed. There, a message's
//! order comes from a UUIDv7 key, because `timestamp` is whole seconds and a
//! fast turn's user and assistant messages would otherwise be indistinguishable.
//! Here the order *is* the file: line N was appended before line N+1.
//!
//! Session metadata stays in the database, on purpose. Titles, status, the model
//! override and the review watermark are all *updated*, and a log is the wrong
//! shape for a value that changes — so each holds what it is good at, and
//! `SessionRepository` reads the two together.
//!
//! ## What holds it together
//!
//! **One writer.** komo runs one turn per session (the gateway dispatcher
//! enforces it), so a transcript has a single writer by construction. The
//! per-session lock here makes that true *within* the process as well, which is
//! what lets the read-modify-write operations below share a file with appends.
//!
//! **A partial line is dropped, never patched.** A process killed mid-append can
//! leave a truncated final line. Reads skip any line that does not parse and say
//! so in the log; nothing tries to repair one, because a half-written message is
//! not a message.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use komo_core::domain::message::Message;
use komo_core::domain::message::Role;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::warn;

/// Bumped when a line's shape changes in a way `serde` defaults cannot absorb.
/// A reader that meets a version it does not know skips the line rather than
/// guessing at it — the same rule the turn journal follows.
const LINE_VERSION: u32 = 1;

/// One line of a transcript file.
///
/// The message's own fields are flattened in, so a line reads as the message it
/// is plus a version. Unknown fields are ignored by serde, which is what lets a
/// newer komo's file still load in an older one.
#[derive(Serialize, Deserialize)]
struct Line {
    #[serde(default = "default_version")]
    v: u32,
    #[serde(flatten)]
    message: Message,
}

fn default_version() -> u32 {
    LINE_VERSION
}

/// Append-only transcript storage, one file per session.
pub struct MessageLog {
    dir: PathBuf,
    /// One lock per session file. Appends and the read-modify-write operations
    /// share it, so a rewrite can never land between another writer's read and
    /// its write.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl MessageLog {
    /// Open (creating it if needed) the transcript directory under `home`.
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        let dir = home.join("sessions");
        std::fs::create_dir_all(&dir).map_err(|e| {
            anyhow::anyhow!(
                "could not create the transcript directory {}: {e}",
                dir.display()
            )
        })?;
        Ok(Self {
            dir,
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// The file a session's transcript lives in.
    ///
    /// Session ids are `{platform}:{chat_id}`, so they carry characters a file
    /// name cannot. Percent-encoding everything outside a conservative set keeps
    /// the mapping reversible and the names readable — `api:1234` becomes
    /// `api%3A1234`, which is still greppable by eye.
    pub fn path_for(&self, session_id: &str) -> PathBuf {
        let mut name = String::with_capacity(session_id.len());
        for byte in session_id.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                    name.push(byte as char)
                }
                other => name.push_str(&format!("%{other:02X}")),
            }
        }
        self.dir.join(format!("{name}.jsonl"))
    }

    async fn lock_for(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks.entry(session_id.to_string()).or_default().clone()
    }

    /// Every message in a session, in the order they were appended.
    pub async fn list(&self, session_id: &str) -> anyhow::Result<Vec<Message>> {
        let path = self.path_for(session_id);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            // A session with no transcript file has no transcript. That is the
            // normal state of a session id a client just minted, not an error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "could not read the transcript {}: {error}",
                    path.display()
                ));
            }
        };
        Ok(parse_lines(&text, &path))
    }

    /// The most recent `limit` messages, still in chronological order.
    /// `limit == 0` means the whole transcript.
    pub async fn window(&self, session_id: &str, limit: usize) -> anyhow::Result<Vec<Message>> {
        let mut all = self.list(session_id).await?;
        if limit > 0 && all.len() > limit {
            all.drain(..all.len() - limit);
        }
        Ok(all)
    }

    /// How many user messages the session holds — the turn count the reviewer's
    /// cadence is sized against.
    pub async fn count_user_turns(&self, session_id: &str) -> anyhow::Result<usize> {
        Ok(self
            .list(session_id)
            .await?
            .iter()
            .filter(|m| m.role == Role::User)
            .count())
    }

    /// Append one message.
    pub async fn append(&self, session_id: &str, message: &Message) -> anyhow::Result<()> {
        let path = self.path_for(session_id);
        let line = serde_json::to_string(&Line {
            v: LINE_VERSION,
            message: message.clone(),
        })?;

        let lock = self.lock_for(session_id).await;
        let _held = lock.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| {
                anyhow::anyhow!("could not open the transcript {}: {e}", path.display())
            })?;
        // One write for the line and its terminator: a reader that catches the
        // file mid-append sees a whole line or none of it.
        file.write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| {
                anyhow::anyhow!("could not append to the transcript {}: {e}", path.display())
            })?;
        // Explicitly, because dropping a `tokio::fs::File` does not flush it —
        // the write is dispatched to a blocking pool and a dropped handle can
        // lose it. Without this the message is in the file only *eventually*,
        // and the next read of the same turn does not find it.
        file.flush().await.map_err(|e| {
            anyhow::anyhow!("could not flush the transcript {}: {e}", path.display())
        })?;
        Ok(())
    }

    /// Drop the `count` most recently appended messages, returning how many were
    /// actually removed.
    pub async fn delete_recent(&self, session_id: &str, count: usize) -> anyhow::Result<usize> {
        if count == 0 {
            return Ok(0);
        }
        let lock = self.lock_for(session_id).await;
        let _held = lock.lock().await;

        let mut messages = self.list(session_id).await?;
        let removed = count.min(messages.len());
        messages.truncate(messages.len() - removed);
        self.rewrite(session_id, &messages).await?;
        Ok(removed)
    }

    /// Append `extra` on its own line to the most recent user message, reporting
    /// whether there was one.
    pub async fn append_to_last_user(&self, session_id: &str, extra: &str) -> anyhow::Result<bool> {
        let lock = self.lock_for(session_id).await;
        let _held = lock.lock().await;

        let mut messages = self.list(session_id).await?;
        let Some(last) = messages.iter_mut().rev().find(|m| m.role == Role::User) else {
            return Ok(false);
        };
        last.content.push('\n');
        last.content.push_str(extra);
        self.rewrite(session_id, &messages).await?;
        Ok(true)
    }

    /// Move a transcript to another session id, as `/new` does when it archives
    /// the conversation it is rotating out of.
    ///
    /// A rename, where the table had to rewrite every row's foreign key.
    pub async fn rename(&self, from: &str, to: &str) -> anyhow::Result<()> {
        let source = self.path_for(from);
        if !source.exists() {
            return Ok(());
        }
        tokio::fs::rename(&source, self.path_for(to))
            .await
            .map_err(|e| {
                anyhow::anyhow!("could not archive the transcript {}: {e}", source.display())
            })
    }

    /// Whether the session has any messages at all — what decides an "empty"
    /// session.
    pub async fn is_empty(&self, session_id: &str) -> anyhow::Result<bool> {
        Ok(self.list(session_id).await?.is_empty())
    }

    /// Delete a session's transcript. Missing is success: the caller wanted it
    /// gone.
    pub async fn remove(&self, session_id: &str) -> anyhow::Result<()> {
        match tokio::fs::remove_file(self.path_for(session_id)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(anyhow::anyhow!("could not remove the transcript: {error}")),
        }
    }

    /// Replace a transcript wholesale, atomically.
    ///
    /// Written to a sibling temp file and renamed, so a crash leaves either the
    /// old transcript or the new one — the two operations that use this
    /// (cancel-rollback and the mid-turn interjection) rewrite a file another
    /// turn may be about to read.
    async fn rewrite(&self, session_id: &str, messages: &[Message]) -> anyhow::Result<()> {
        let path = self.path_for(session_id);
        let mut body = String::new();
        for message in messages {
            body.push_str(&serde_json::to_string(&Line {
                v: LINE_VERSION,
                message: message.clone(),
            })?);
            body.push('\n');
        }
        let temp = path.with_extension("jsonl.tmp");
        tokio::fs::write(&temp, body)
            .await
            .map_err(|e| anyhow::anyhow!("could not stage the transcript rewrite: {e}"))?;
        tokio::fs::rename(&temp, &path)
            .await
            .map_err(|e| anyhow::anyhow!("could not replace the transcript: {e}"))
    }
}

/// Parse a transcript, skipping anything that does not read as a message.
///
/// A line can fail to parse for two reasons, and both mean the same thing here:
/// the process died mid-append and left a truncated tail, or the line was
/// written by a komo whose format this one does not know. Neither is repairable
/// and neither should cost the rest of the transcript — but a dropped message is
/// a real loss, so it is never silent.
fn parse_lines(text: &str, path: &Path) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(line) {
            Ok(parsed) if parsed.v <= LINE_VERSION => messages.push(parsed.message),
            Ok(parsed) => {
                warn!(
                    path = %path.display(),
                    version = parsed.v,
                    "transcript line written by a newer komo; skipped"
                );
                skipped += 1;
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "unreadable transcript line; skipped");
                skipped += 1;
            }
        }
    }
    if skipped > 0 {
        warn!(
            path = %path.display(),
            skipped,
            "transcript loaded with lines missing"
        );
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(tag: &str) -> (MessageLog, PathBuf) {
        let home = std::env::temp_dir().join(format!("komo-msglog-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&home).unwrap();
        (MessageLog::open(&home).unwrap(), home)
    }

    fn assistant(content: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: content.to_string(),
            timestamp: 0,
            tool_note: String::new(),
        }
    }

    #[tokio::test]
    async fn messages_come_back_in_the_order_they_were_appended() {
        let (log, _home) = log("order");
        // Same timestamp on purpose: in the table this is exactly the case that
        // forced a UUIDv7 key, because `timestamp` is whole seconds. Here the
        // file's order is the answer.
        for text in ["one", "two", "three"] {
            log.append("api:s", &assistant(text)).await.unwrap();
        }
        let got: Vec<String> = log
            .list("api:s")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(got, ["one", "two", "three"]);
    }

    #[tokio::test]
    async fn a_session_with_no_file_has_an_empty_transcript() {
        let (log, _home) = log("missing");
        assert!(log.list("api:never-used").await.unwrap().is_empty());
        assert!(log.is_empty("api:never-used").await.unwrap());
    }

    #[tokio::test]
    async fn the_window_keeps_the_last_n_in_order() {
        let (log, _home) = log("window");
        for text in ["a", "b", "c", "d"] {
            log.append("api:s", &assistant(text)).await.unwrap();
        }
        let got: Vec<String> = log
            .window("api:s", 2)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(got, ["c", "d"], "the most recent two, oldest first");

        // 0 is "no window", the same meaning `find_windowed` gives it.
        assert_eq!(log.window("api:s", 0).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn user_turns_are_counted_without_the_transcript_shape_mattering() {
        let (log, _home) = log("count");
        log.append("api:s", &Message::user("hi")).await.unwrap();
        log.append("api:s", &assistant("hello")).await.unwrap();
        log.append("api:s", &Message::user("again")).await.unwrap();
        assert_eq!(log.count_user_turns("api:s").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn deleting_recent_messages_takes_them_off_the_end() {
        let (log, _home) = log("delete");
        for text in ["a", "b", "c"] {
            log.append("api:s", &assistant(text)).await.unwrap();
        }
        assert_eq!(log.delete_recent("api:s", 2).await.unwrap(), 2);
        let left: Vec<String> = log
            .list("api:s")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(left, ["a"]);

        // Asking for more than there are removes what there is.
        assert_eq!(log.delete_recent("api:s", 9).await.unwrap(), 1);
        assert!(log.list("api:s").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_interjection_joins_the_last_user_message() {
        let (log, _home) = log("interject");
        log.append("api:s", &Message::user("do the thing"))
            .await
            .unwrap();
        log.append("api:s", &assistant("working")).await.unwrap();

        assert!(
            log.append_to_last_user("api:s", "wait, also this")
                .await
                .unwrap()
        );
        let messages = log.list("api:s").await.unwrap();
        assert_eq!(messages[0].content, "do the thing\nwait, also this");
        assert_eq!(messages.len(), 2, "the assistant message is untouched");

        // Nothing to append to is reported, not invented.
        assert!(!log.append_to_last_user("api:other", "x").await.unwrap());
    }

    #[tokio::test]
    async fn rotating_moves_the_transcript_to_the_archived_id() {
        let (log, _home) = log("rotate");
        log.append("api:s", &assistant("old talk")).await.unwrap();
        log.rename("api:s", "api:s-archived").await.unwrap();

        assert!(log.list("api:s").await.unwrap().is_empty());
        assert_eq!(
            log.list("api:s-archived").await.unwrap()[0].content,
            "old talk"
        );
    }

    /// A process killed mid-append leaves a truncated last line. The rest of the
    /// transcript has to survive it — losing one message is bad, losing the
    /// conversation because of one message is worse.
    #[tokio::test]
    async fn a_truncated_final_line_costs_only_itself() {
        let (log, _home) = log("torn");
        log.append("api:s", &assistant("first")).await.unwrap();
        log.append("api:s", &assistant("second")).await.unwrap();

        let path = log.path_for("api:s");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"v\":1,\"role\":\"assist");
        std::fs::write(&path, text).unwrap();

        let got: Vec<String> = log
            .list("api:s")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(got, ["first", "second"]);
    }

    /// The point of the format: a field added later reads as its default on
    /// every line written before it existed, with no migration and no reset.
    #[tokio::test]
    async fn a_line_missing_a_later_field_still_loads() {
        let (log, _home) = log("additive");
        let path = log.path_for("api:s");
        // A line as an older komo would have written it: no `tool_note`.
        std::fs::write(
            &path,
            "{\"v\":1,\"role\":\"assistant\",\"content\":\"hi\",\"timestamp\":7}\n",
        )
        .unwrap();

        let messages = log.list("api:s").await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "hi");
        assert_eq!(messages[0].tool_note, "", "absent reads as the default");
    }

    /// A line from a komo that writes a shape this one does not understand is
    /// skipped rather than half-read.
    #[tokio::test]
    async fn a_line_from_a_newer_komo_is_skipped() {
        let (log, _home) = log("newer");
        let path = log.path_for("api:s");
        std::fs::write(
            &path,
            "{\"v\":99,\"role\":\"assistant\",\"content\":\"from the future\",\"timestamp\":0}\n\
             {\"v\":1,\"role\":\"assistant\",\"content\":\"mine\",\"timestamp\":0}\n",
        )
        .unwrap();

        let messages = log.list("api:s").await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "mine");
    }

    /// Session ids carry `:` and whatever a chat platform puts in an id; the
    /// file name has to survive that and stay reversible.
    #[test]
    fn session_ids_become_readable_file_names() {
        let (log, home) = log("names");
        assert_eq!(
            log.path_for("api:1234"),
            home.join("sessions").join("api%3A1234.jsonl")
        );
        assert_eq!(
            log.path_for("feishu:oc_9/x"),
            home.join("sessions").join("feishu%3Aoc_9%2Fx.jsonl")
        );
    }
}
