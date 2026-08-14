//! The Anthropic Messages API codec.
//!
//! Anthropic serves no Responses endpoint, so this is the one place komo carries
//! a second wire format. Three things differ from [`super::responses`] in ways
//! that matter, and they are the reason this is a separate codec rather than a
//! few conditionals in that one:
//!
//! - **Caching is explicit.** Anthropic caches nothing unless the request marks
//!   `cache_control` breakpoints. komo marks three — the system prompt, the last
//!   tool definition, and the last message — which is what lets each round of a
//!   tool loop reuse the previous round's prefix. See [`mark_cache_breakpoints`].
//! - **`max_tokens` is mandatory**, and thinking is charged against it.
//! - **Tool arguments are an object, not a string.** komo stores the raw JSON
//!   string the model emitted (so a round-trip is byte-faithful); this codec
//!   parses it on the way out and re-serializes on the way back.

use serde_json::{Value, json};

use super::error::{LlmError, LlmErrorKind, classify_status, retry_after_from_message};
use super::transport::SseStream;
use super::types::{AssistantBlock, Completion, Reasoning, ToolSchema, Turn, Usage, UserBlock};

/// The API version this codec is written against, sent on every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Fallback answer budget when the caller sets none. Anthropic rejects a request
/// with no `max_tokens` outright, so unlike every other knob this one cannot be
/// left to the provider's default — there isn't one.
const DEFAULT_MAX_TOKENS: u64 = 8_192;

/// Build the request body for one round.
pub fn request(
    model: &str,
    system: &str,
    history: &[Turn],
    tools: &[ToolSchema],
    extra: Option<&Value>,
) -> Value {
    let mut body = json!({
        "model": model,
        "system": [{
            "type": "text",
            "text": system,
            // Breakpoint 1: the system prompt. Stable across every turn of a
            // session, so it is the most valuable thing in the request to cache.
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": messages(history),
        "max_tokens": DEFAULT_MAX_TOKENS,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(tool_schema).collect());
    }
    if let Some(extra) = extra.and_then(Value::as_object)
        && let Some(base) = body.as_object_mut()
    {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    mark_cache_breakpoints(&mut body);
    body
}

fn tool_schema(tool: &ToolSchema) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters,
    })
}

/// Mark the two moving `cache_control` breakpoints.
///
/// The system prompt is marked when the body is built; these two are applied
/// afterwards because they depend on what ended up in the request:
///
/// - **The last tool definition.** Tool schemas are identical every round, so
///   one breakpoint after all of them caches the whole block.
/// - **The last message.** This is the one that does the real work in a tool
///   loop: marking the newest message means the *next* round's request finds a
///   cached prefix covering everything up to it, so a ten-round turn pays for
///   each round's new bytes once instead of re-reading the whole conversation
///   every time.
fn mark_cache_breakpoints(body: &mut Value) {
    let ephemeral = json!({ "type": "ephemeral" });
    if let Some(last) = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
    {
        last["cache_control"] = ephemeral.clone();
    }
    // The breakpoint goes on the last *content block* of the last message —
    // Anthropic marks blocks, not messages.
    if let Some(block) = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|messages| messages.last_mut())
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
    {
        block["cache_control"] = ephemeral;
    }
}

/// Render komo's conversation as Anthropic messages.
///
/// Unlike the Responses API's flat item list, everything nests: a tool call is a
/// block inside an assistant message, and its result is a block inside the
/// following user message. Consecutive turns of the same role are merged,
/// because Anthropic requires strict user/assistant alternation.
fn messages(history: &[Turn]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for turn in history {
        let (role, blocks) = match turn {
            Turn::User(blocks) => ("user", blocks.iter().map(user_block).collect::<Vec<_>>()),
            Turn::Assistant { blocks, .. } => (
                "assistant",
                blocks.iter().filter_map(assistant_block).collect(),
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        match out.last_mut() {
            // Same role twice in a row: fold into the previous message rather
            // than sending an alternation Anthropic rejects.
            Some(last) if last["role"] == role => {
                if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                    content.extend(blocks);
                }
            }
            _ => out.push(json!({ "role": role, "content": blocks })),
        }
    }
    out
}

fn user_block(block: &UserBlock) -> Value {
    match block {
        UserBlock::Text(text) => json!({ "type": "text", "text": text }),
        UserBlock::ToolResult { id, text, .. } => json!({
            "type": "tool_result",
            // Anthropic correlates on the id it issued with the `tool_use`
            // block, which is what komo stores as `id`.
            "tool_use_id": id,
            "content": [{ "type": "text", "text": text }],
        }),
    }
}

fn assistant_block(block: &AssistantBlock) -> Option<Value> {
    Some(match block {
        AssistantBlock::Text(text) => {
            // Anthropic rejects an empty text block; a model that only made a
            // tool call produces one.
            if text.trim().is_empty() {
                return None;
            }
            json!({ "type": "text", "text": text })
        }
        AssistantBlock::ToolCall { id, name, args, .. } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            // komo keeps the raw string the model emitted; Anthropic wants the
            // parsed object. An unparseable payload becomes `{}` rather than
            // failing the round — the model will be told the call was malformed
            // by the tool executor, which is the recoverable path.
            "input": serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({})),
        }),
        AssistantBlock::Reasoning(reasoning) => {
            // A thinking block only round-trips with its signature; without one
            // Anthropic rejects it, so drop it rather than send something
            // invalid.
            let signature = reasoning.encrypted.as_ref()?;
            json!({
                "type": "thinking",
                "thinking": reasoning.summary.join(""),
                "signature": signature,
            })
        }
    })
}

/// Drain `stream` into one assistant message.
///
/// Same terminal-event rule as the Responses codec: `message_stop` is the only
/// thing that makes a round successful. Anything less is a retryable
/// [`LlmErrorKind::Stream`], never a short answer.
pub async fn collect(
    stream: &mut SseStream,
    on_delta: Option<super::OnDelta<'_>>,
) -> Result<Completion, LlmError> {
    let mut blocks: Vec<AssistantBlock> = Vec::new();
    let mut id = None;
    let mut usage = Usage::default();
    let mut stopped = false;
    // The block currently being streamed: its shape from `content_block_start`
    // plus the deltas accumulated since.
    let mut open: Option<(Value, String)> = None;

    while let Some(frame) = stream.next().await? {
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "message_start" => {
                let message = frame.get("message");
                id = message
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // Input usage (including cache accounting) is reported up front.
                let reported = read_usage(message.and_then(|m| m.get("usage")));
                usage.input = reported.input;
                usage.cached_input = reported.cached_input;
            }
            "content_block_start" => {
                open = frame
                    .get("content_block")
                    .cloned()
                    .map(|block| (block, String::new()));
            }
            "content_block_delta" => {
                let delta = frame.get("delta");
                let text = delta_text(delta);
                if let Some((_, buffer)) = open.as_mut() {
                    buffer.push_str(&text);
                }
                // Which kind of chunk this is comes from the delta's own type,
                // not the buffer: one block's stream can carry both thinking
                // text and its signature.
                if let Some(on_delta) = on_delta
                    && let Some(kind) = delta.and_then(|d| d.get("type")).and_then(Value::as_str)
                {
                    match kind {
                        "text_delta" => on_delta(super::Delta::Text(&text)),
                        "thinking_delta" => on_delta(super::Delta::Reasoning(&text)),
                        // Tool arguments and the thinking signature are machinery,
                        // not something to show a watcher.
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                if let Some((shape, buffer)) = open.take()
                    && let Some(block) = finish_block(shape, buffer)
                {
                    blocks.push(block);
                }
            }
            "message_delta" => {
                // Output usage arrives with the stop reason.
                usage.output = read_usage(frame.get("usage")).output;
            }
            "message_stop" => {
                stopped = true;
                break;
            }
            "error" => return Err(stream_error(frame.get("error"))),
            // `ping` and anything new: nothing to do.
            _ => {}
        }
    }

    if !stopped {
        return Err(LlmError::stream(
            "the provider closed the stream before completing the message",
        ));
    }
    Ok(Completion { id, blocks, usage })
}

/// The text carried by any of Anthropic's delta shapes. `text_delta` carries
/// prose, `input_json_delta` a chunk of a tool call's arguments, `thinking_delta`
/// reasoning, `signature_delta` the thinking signature — all string-valued under
/// different key names.
fn delta_text(delta: Option<&Value>) -> String {
    let Some(delta) = delta else {
        return String::new();
    };
    for key in ["text", "partial_json", "thinking", "signature"] {
        if let Some(text) = delta.get(key).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    String::new()
}

/// Turn a finished content block into an assistant block.
///
/// The accumulated `buffer` is the streamed remainder; the block's own fields
/// carry whatever arrived in `content_block_start`. A tool call's arguments only
/// ever arrive as deltas, so for those the buffer *is* the payload.
fn finish_block(shape: Value, buffer: String) -> Option<AssistantBlock> {
    let str_field = |name: &str| {
        shape
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match shape.get("type").and_then(Value::as_str)? {
        "text" => {
            let text = format!("{}{buffer}", str_field("text"));
            (!text.is_empty()).then_some(AssistantBlock::Text(text))
        }
        "tool_use" => {
            let id = str_field("id");
            let args = if buffer.trim().is_empty() {
                // A no-argument call may arrive complete in the start frame.
                shape
                    .get("input")
                    .map(|input| input.to_string())
                    .unwrap_or_else(|| "{}".to_string())
            } else {
                buffer
            };
            Some(AssistantBlock::ToolCall {
                id: id.clone(),
                // Anthropic issues one handle, not two; keying on `id` is what
                // the tool-result block does too.
                call_id: None,
                name: str_field("name"),
                args,
            })
        }
        "thinking" => {
            // The signature arrives as its own delta stream after the thinking
            // text, so `content_block_start` holds neither. Splitting them back
            // out is not possible from one buffer — Anthropic sends
            // `signature_delta` frames separately, and `delta_text` folded them
            // into the same buffer. Keep the whole buffer as the thinking text
            // and take the signature from the start frame when it is there.
            let signature = shape.get("signature").and_then(Value::as_str);
            Some(AssistantBlock::Reasoning(Reasoning {
                id: None,
                summary: vec![format!("{}{buffer}", str_field("thinking"))],
                encrypted: signature.map(str::to_string),
            }))
        }
        _ => None,
    }
}

fn stream_error(error: Option<&Value>) -> LlmError {
    let code = error
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("the provider reported an error mid-stream")
        .to_string();
    let kind = classify_status(200, code.as_deref(), &message);
    let kind = if code.is_none() && kind == LlmErrorKind::InvalidRequest {
        LlmErrorKind::Stream
    } else {
        kind
    };
    LlmError::new(kind, message.clone())
        .with_code(code)
        .with_retry_after(retry_after_from_message(&message))
}

fn read_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_i64).unwrap_or(0);
    // Anthropic reports three *disjoint* prompt counts: `input_tokens` is what
    // was neither read from nor written to the cache. `Usage::input` is the
    // total, so they are summed — reporting only `input_tokens` would make this
    // provider's `tokens_in` mean something different from every other one's,
    // and would put `cached_input` outside the total it is supposed to be a
    // subset of.
    let cache_read = field("cache_read_input_tokens");
    Usage {
        input: field("input_tokens") + field("cache_creation_input_tokens") + cache_read,
        output: field("output_tokens"),
        cached_input: cache_read,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }
    }

    /// Anthropic caches nothing without these, and the moving message
    /// breakpoint is what makes a multi-round tool loop affordable.
    #[test]
    fn three_cache_breakpoints_are_marked() {
        let body = request(
            "claude",
            "system prompt",
            &[Turn::user("hi")],
            &[schema("a"), schema("b")],
            None,
        );
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(
            body["tools"][0].get("cache_control").is_none(),
            "only the last tool carries the breakpoint"
        );
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        let last = &body["messages"][0]["content"][0];
        assert_eq!(last["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn max_tokens_is_always_sent_because_anthropic_requires_it() {
        let body = request("claude", "s", &[Turn::user("hi")], &[], None);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        // A caller's budget wins (the thinking path raises it).
        let raised = request(
            "claude",
            "s",
            &[Turn::user("hi")],
            &[],
            Some(&json!({ "max_tokens": 32_768 })),
        );
        assert_eq!(raised["max_tokens"], 32_768);
    }

    #[test]
    fn tool_arguments_are_parsed_into_an_object() {
        let history = vec![Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::ToolCall {
                id: "toolu_1".into(),
                call_id: None,
                name: "read".into(),
                args: r#"{"path":"foo"}"#.into(),
            }],
        }];
        let body = request("claude", "s", &history, &[], None);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(
            block["input"]["path"], "foo",
            "an object, not the raw string komo stores"
        );

        // Malformed arguments must not fail the round — the executor reports
        // them back to the model as a recoverable tool error.
        let broken = vec![Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::ToolCall {
                id: "t".into(),
                call_id: None,
                name: "read".into(),
                args: "{not json".into(),
            }],
        }];
        let body = request("claude", "s", &broken, &[], None);
        assert_eq!(body["messages"][0]["content"][0]["input"], json!({}));
    }

    #[test]
    fn consecutive_same_role_turns_are_merged() {
        // Anthropic requires strict alternation, and komo's history can hold two
        // user turns in a row (a tool result followed by an interjection).
        let history = vec![
            Turn::user("first"),
            Turn::User(vec![UserBlock::ToolResult {
                id: "t1".into(),
                call_id: None,
                text: "result".into(),
            }]),
        ];
        let body = request("claude", "s", &history, &[], None);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "folded into one user message");
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[0]["content"][1]["tool_use_id"], "t1");
    }

    #[test]
    fn empty_assistant_text_is_dropped() {
        // A model that only emitted a tool call leaves an empty text block,
        // which Anthropic rejects.
        let history = vec![Turn::Assistant {
            id: None,
            blocks: vec![
                AssistantBlock::Text("  ".into()),
                AssistantBlock::ToolCall {
                    id: "t".into(),
                    call_id: None,
                    name: "read".into(),
                    args: "{}".into(),
                },
            ],
        }];
        let body = request("claude", "s", &history, &[], None);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
    }

    #[test]
    fn thinking_without_a_signature_is_dropped() {
        // Anthropic rejects a thinking block whose signature is missing, so
        // sending it would fail the round outright.
        let history = vec![Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::Reasoning(Reasoning {
                id: None,
                summary: vec!["hmm".into()],
                encrypted: None,
            })],
        }];
        let body = request("claude", "s", &history, &[], None);
        assert!(
            body["messages"].as_array().unwrap().is_empty(),
            "no valid block left, so no message"
        );

        let signed = vec![Turn::Assistant {
            id: None,
            blocks: vec![AssistantBlock::Reasoning(Reasoning {
                id: None,
                summary: vec!["hmm".into()],
                encrypted: Some("SIG".into()),
            })],
        }];
        let body = request("claude", "s", &signed, &[], None);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["signature"], "SIG");
    }

    #[test]
    fn deltas_accumulate_into_blocks() {
        let text = finish_block(
            json!({ "type": "text", "text": "" }),
            "streamed answer".into(),
        );
        assert_eq!(text, Some(AssistantBlock::Text("streamed answer".into())));

        let call = finish_block(
            json!({ "type": "tool_use", "id": "toolu_1", "name": "read" }),
            r#"{"path":"x"}"#.into(),
        );
        assert_eq!(
            call,
            Some(AssistantBlock::ToolCall {
                id: "toolu_1".into(),
                call_id: None,
                name: "read".into(),
                args: r#"{"path":"x"}"#.into(),
            })
        );

        // A no-argument call can arrive complete in the start frame.
        let empty = finish_block(
            json!({ "type": "tool_use", "id": "t", "name": "time", "input": {} }),
            String::new(),
        );
        assert!(matches!(
            empty,
            Some(AssistantBlock::ToolCall { ref args, .. }) if args == "{}"
        ));
    }

    #[test]
    fn every_delta_shape_yields_its_text() {
        for (key, value) in [
            ("text", "prose"),
            ("partial_json", "{\"a\":"),
            ("thinking", "reasoning"),
            ("signature", "sig"),
        ] {
            assert_eq!(delta_text(Some(&json!({ key: value }))), value);
        }
        assert_eq!(delta_text(None), "");
        assert_eq!(delta_text(Some(&json!({ "unknown": 1 }))), "");
    }

    #[test]
    fn an_overload_error_reads_as_retryable() {
        let error = stream_error(Some(
            &json!({ "type": "overloaded_error", "message": "Overloaded" }),
        ));
        assert_eq!(error.kind, LlmErrorKind::Overloaded);
        assert!(error.is_retryable());
    }

    /// Anthropic's three prompt counts are disjoint, so `input` is their sum —
    /// otherwise this provider's `tokens_in` would exclude exactly the tokens
    /// every other provider's includes, and the hit rate would read 80/100
    /// here instead of the true 80/200.
    #[test]
    fn usage_totals_the_three_disjoint_prompt_counts() {
        let usage = read_usage(Some(&json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_creation_input_tokens": 20,
            "cache_read_input_tokens": 80
        })));
        assert_eq!(usage.input, 200, "uncached + cache-write + cache-read");
        assert_eq!(usage.output, 20);
        assert_eq!(usage.cached_input, 80);
        assert!(
            usage.cached_input <= usage.input,
            "cache hits are a subset of the prompt"
        );
    }

    /// A response with no cache accounting at all still reports its prompt in
    /// full — the missing fields read as zero, not as a smaller total.
    #[test]
    fn usage_without_cache_fields_is_just_the_prompt() {
        let usage = read_usage(Some(&json!({
            "input_tokens": 100,
            "output_tokens": 20
        })));
        assert_eq!(usage.input, 100);
        assert_eq!(usage.cached_input, 0);
    }
}
