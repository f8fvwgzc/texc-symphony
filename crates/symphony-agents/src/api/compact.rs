//! Compaction of old tool results in the API loop's history.
//!
//! The loop re-sends its whole conversation on every model call, and tool results (command output,
//! file contents) are most of it. Once the estimated conversation passes the backend's
//! `compact_after_tokens`, every tool result except the [`KEEP_RECENT_RESULTS`] newest
//! `tool_result` messages is replaced by a short stub that keeps its first line and says how to get
//! it back (run the tool again). Calls, the model's text and short results stay as they were.
//!
//! The cut happens all at once, so the conversation falls well below the threshold and the next
//! cut is far away: each one invalidates the provider's prompt cache from the first changed
//! result on, and the cache is rebuilt from there by the next call. Stubs are shorter than
//! [`MIN_CUT_CHARS`], so a cut never touches a result twice.
//!
//! Sizes are estimates (about four characters per token), counted over the system prompt, the
//! tool definitions and the messages.

use super::wire::{Message, ToolDef};

/// `tool_result` messages, newest first, that are never cut.
pub const KEEP_RECENT_RESULTS: usize = 4;
/// Results shorter than this many characters are kept.
pub const MIN_CUT_CHARS: usize = 1024;
/// Characters of a cut result kept in its stub.
const STUB_HEAD_CHARS: usize = 200;

/// What one compaction removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Compacted {
    /// Tool results replaced by a stub.
    pub results: usize,
    /// Characters removed.
    pub chars: usize,
}

/// Estimated tokens of a request with `system`, `tools` and `messages`.
pub fn estimate_tokens(system: &str, tools: &[ToolDef], messages: &[Message]) -> u64 {
    let tools: usize = tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.schema.to_string().len())
        .sum();
    let messages: usize = messages
        .iter()
        .map(|message| match message {
            Message::User(text) => text.len(),
            Message::Assistant { text, tool_calls } => {
                text.len()
                    + tool_calls
                        .iter()
                        .map(|call| {
                            call.name.len()
                                + match &call.arguments {
                                    Ok(args) => args.to_string().len(),
                                    Err(raw) => raw.len(),
                                }
                        })
                        .sum::<usize>()
            }
            Message::ToolResults(results) => results.iter().map(|r| r.content.len()).sum(),
        })
        .sum();
    u64::try_from((system.len() + tools + messages) / 4).unwrap_or(u64::MAX)
}

/// The stub that replaces a cut result of `chars` characters.
fn stub(content: &str, chars: usize) -> String {
    let first_line = content.lines().next().unwrap_or("");
    let head: String = first_line.chars().take(STUB_HEAD_CHARS).collect();
    format!(
        "[Tool result removed from the history to save context ({chars} characters). Run the tool \
         again if you need it. It began: {head}]"
    )
}

/// Cuts every tool result longer than [`MIN_CUT_CHARS`] except those of the
/// [`KEEP_RECENT_RESULTS`] newest `tool_result` messages.
pub fn compact(messages: &mut [Message]) -> Compacted {
    let mut done = Compacted::default();
    let older = messages
        .iter_mut()
        .rev()
        .filter_map(|message| match message {
            Message::ToolResults(results) => Some(results),
            _ => None,
        })
        .skip(KEEP_RECENT_RESULTS);
    for results in older {
        for result in results.iter_mut() {
            let chars = result.content.chars().count();
            if chars < MIN_CUT_CHARS {
                continue;
            }
            let stub = stub(&result.content, chars);
            done.chars += chars.saturating_sub(stub.chars().count());
            done.results += 1;
            result.content = stub;
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::wire::{ToolCall, ToolOutcome};

    fn exchange(id: usize, content: &str) -> [Message; 2] {
        [
            Message::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("c{id}"),
                    name: "shell".into(),
                    arguments: Ok(json!({"command": "make"})),
                }],
            },
            Message::ToolResults(vec![ToolOutcome {
                id: format!("c{id}"),
                content: content.to_owned(),
                is_error: false,
            }]),
        ]
    }

    fn result(message: &Message) -> &str {
        match message {
            Message::ToolResults(results) => &results[0].content,
            other => panic!("not a tool result: {other:?}"),
        }
    }

    #[test]
    fn older_long_results_become_stubs_and_the_newest_stay() {
        let long = format!("Compiling symphony\n{}", "x".repeat(5000));
        let mut messages = vec![Message::User("build it".into())];
        for id in 0..6 {
            messages.extend(exchange(id, &long));
        }
        messages.extend(exchange(6, "ok"));
        let before = estimate_tokens("sys", &[], &messages);
        let done = compact(&mut messages);
        // Seven result messages: the four newest are kept, so results 0, 1 and 2 are cut.
        assert_eq!(done.results, 3);
        assert!(result(&messages[2]).starts_with("[Tool result removed"));
        assert!(result(&messages[2]).contains("It began: Compiling symphony]"));
        assert!(result(&messages[2]).contains("(5019 characters)"));
        assert_eq!(
            result(&messages[8]),
            long,
            "the fourth newest result is kept"
        );
        assert_eq!(result(&messages[14]), "ok");
        let after = estimate_tokens("sys", &[], &messages);
        assert!(
            before - after + 1 >= (done.chars / 4) as u64,
            "{before} -> {after}"
        );
        // Stubs are below the cut size: a second pass changes nothing.
        let again = messages.clone();
        assert_eq!(compact(&mut messages), Compacted::default());
        assert_eq!(messages, again);
    }

    #[test]
    fn short_results_calls_and_text_are_kept() {
        let mut messages = vec![Message::User("u".repeat(4000))];
        for id in 0..6 {
            messages.extend(exchange(id, "short output"));
        }
        let kept = messages.clone();
        assert_eq!(compact(&mut messages), Compacted::default());
        assert_eq!(messages, kept);
        assert_eq!(estimate_tokens("", &[], &messages[..1]), 1000);
    }
}
