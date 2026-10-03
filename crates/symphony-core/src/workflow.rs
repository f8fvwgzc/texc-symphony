//! `WORKFLOW.md` loading: YAML front matter + prompt body (`SymphonyElixir.Workflow`).

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use crate::config::value::yaml_to_json;
use crate::error::{ConfigError, IoReason};

/// File name looked up in the working directory when no path is configured.
pub const WORKFLOW_FILE_NAME: &str = "WORKFLOW.md";

/// A parsed workflow file.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LoadedWorkflow {
    /// Decoded front matter with stringified keys (not yet cast; see [`crate::config::parse`]).
    pub config: Map<String, Value>,
    /// The trimmed prompt body.
    pub prompt: String,
    /// Same as `prompt` (kept for Elixir API parity).
    pub prompt_template: String,
}

/// `Workflow.load/1`: reads and parses `path`.
pub fn load(path: &Path) -> Result<LoadedWorkflow, ConfigError> {
    let bytes = fs::read(path).map_err(|err| ConfigError::MissingWorkflowFile {
        path: path.to_path_buf(),
        reason: IoReason::from_io(&err),
    })?;
    let content = String::from_utf8(bytes)
        .map_err(|_| ConfigError::WorkflowParseError("workflow file is not valid UTF-8".into()))?;
    parse(&content)
}

/// Splits on line breaks like Elixir's non-unicode `~r/\R/` (CRLF, LF, CR, VT, FF).
fn split_lines(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                lines.push(&content[start..i]);
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            b'\n' | 0x0B | 0x0C => {
                lines.push(&content[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    lines.push(&content[start..]);
    lines
}

fn split_front_matter(content: &str) -> (Vec<&str>, Vec<&str>) {
    let lines = split_lines(content);
    match lines.split_first() {
        Some((&"---", tail)) => match tail.iter().position(|line| *line == "---") {
            Some(close) => (tail[..close].to_vec(), tail[close + 1..].to_vec()),
            // Unterminated front matter: everything after the opener is YAML, the prompt is empty.
            None => (tail.to_vec(), Vec::new()),
        },
        _ => (Vec::new(), lines),
    }
}

fn front_matter_to_map(lines: &[&str]) -> Result<Map<String, Value>, ConfigError> {
    let yaml = lines.join("\n");
    if yaml.trim().is_empty() {
        return Ok(Map::new());
    }
    let decoded: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml)
        .map_err(|err| ConfigError::WorkflowParseError(err.to_string()))?;
    match yaml_to_json(decoded) {
        Value::Object(map) => Ok(map),
        // A comment-only document decodes to null; yamerl yields an empty map, so treat it the same.
        Value::Null => Ok(Map::new()),
        _ => Err(ConfigError::WorkflowFrontMatterNotAMap),
    }
}

/// Parses workflow file content.
///
/// If the first line is exactly `---`, the lines up to the next exact `---` are YAML front matter and the
/// rest is the prompt; without a closing `---` everything after the opener is front matter and the prompt
/// is empty. Otherwise the whole file is the prompt. The prompt is re-joined with `\n` (CRLF becomes LF)
/// and trimmed.
pub fn parse(content: &str) -> Result<LoadedWorkflow, ConfigError> {
    let (front, prompt_lines) = split_front_matter(content);
    let config = front_matter_to_map(&front)?;
    let prompt = prompt_lines.join("\n").trim().to_owned();
    Ok(LoadedWorkflow {
        config,
        prompt_template: prompt.clone(),
        prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_elixir_r_escape() {
        assert_eq!(
            split_lines("a\r\nb\rc\nd\x0be\x0cf"),
            vec!["a", "b", "c", "d", "e", "f"]
        );
        assert_eq!(split_lines(""), vec![""]);
        assert_eq!(split_lines("x\n"), vec!["x", ""]);
        // Multi-byte characters are never split (Elixir would split on a raw 0x85 byte).
        assert_eq!(split_lines("aą\nb"), vec!["aą", "b"]);
    }
}
