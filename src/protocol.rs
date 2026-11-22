//! The `claude` program's JSON stream protocol, as the Claude Agent SDK
//! speaks it: one JSON object per line each way.
//!
//! The program runs with `--print --input-format stream-json
//! --output-format stream-json`. Into it go the user's messages and control
//! requests (`initialize`, `interrupt`, `set_model`, `set_permission_mode`,
//! `apply_flag_settings` for the effort level, `get_settings`), and the answers to its own control requests: with
//! `--permission-prompt-tool stdio`, before it uses a tool that needs the
//! user's permission it asks with a `can_use_tool` request, which the
//! extension answers `allow` (possibly with changed input, and permission
//! rules to remember) or `deny`. Out of it come `system`, `assistant`, `user`
//! (tool results), `stream_event` (the model's output as it streams, with
//! `--include-partial-messages`), `result` (the end of a turn), and
//! `control_response` messages.
//!
//! Nothing here calls the host: these are the messages' shapes.

use serde_json::{Value, json};

/// How the program is started for a chat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Start {
  /// A model or alias (`sonnet`); Claude Code's default without one.
  pub model: Option<String>,
  /// `default`, `acceptEdits`, `plan`, or `bypassPermissions`.
  pub mode: Option<String>,
  pub effort: Option<String>,
  /// A session to continue.
  pub resume: Option<String>,
}

/// The program's arguments for a chat.
pub fn arguments(start: &Start) -> Vec<String> {
  let mut args: Vec<String> = [
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--permission-prompt-tool",
    "stdio",
  ]
  .map(str::to_owned)
  .to_vec();
  let mut option = |name: &str, value: &Option<String>| {
    if let Some(value) = value
      .as_deref()
      .map(str::trim)
      .filter(|value| !value.is_empty())
    {
      args.push(name.to_owned());
      args.push(value.to_owned());
    }
  };
  option("--model", &start.model);
  option("--permission-mode", &start.mode);
  option("--effort", &start.effort);
  option("--resume", &start.resume);
  args
}

/// A line to write: `message` as JSON, and a line break.
fn line(message: &Value) -> Vec<u8> {
  let mut bytes = serde_json::to_vec(message).unwrap_or_default();
  bytes.push(b'\n');
  bytes
}

/// A message from the user: content blocks, text first.
pub fn user_message(blocks: Vec<Value>) -> Vec<u8> {
  line(&json!({
    "type": "user",
    "message": { "role": "user", "content": blocks },
    "parent_tool_use_id": null,
    "session_id": "",
  }))
}

/// A text content block.
pub fn text_block(text: &str) -> Value {
  json!({ "type": "text", "text": text })
}

/// A control request of the extension's, which the program answers with a
/// `control_response` naming `id`.
pub fn control_request(id: &str, request: Value) -> Vec<u8> {
  line(&json!({ "type": "control_request", "request_id": id, "request": request }))
}

pub fn initialize() -> Value {
  json!({ "subtype": "initialize" })
}

pub fn interrupt() -> Value {
  json!({ "subtype": "interrupt" })
}

pub fn set_model(model: &str) -> Value {
  json!({ "subtype": "set_model", "model": model })
}

pub fn set_permission_mode(mode: &str) -> Value {
  json!({ "subtype": "set_permission_mode", "mode": mode })
}

/// Changes the effort level for the rest of the session; `None` goes back to
/// the model's default (or the user's Claude Code settings).
pub fn set_effort(level: Option<&str>) -> Value {
  json!({ "subtype": "apply_flag_settings", "settings": { "effortLevel": level } })
}

/// Asks for the settings in effect; the answer's `applied.effort` is the
/// effort level the next turn uses.
pub fn get_settings() -> Value {
  json!({ "subtype": "get_settings" })
}

/// How the user answered a `can_use_tool` request.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
  /// Use the tool, with this input (the request's, or changed: a question's
  /// answers). `remember` are permission rules to keep for the session,
  /// such as those the request suggested.
  Allow {
    input: Value,
    remember: Option<Value>,
  },
  /// Don't, telling Claude why.
  Deny { message: String },
}

/// The answer to one of the program's control requests.
pub fn control_response(request_id: &str, response: Value) -> Vec<u8> {
  line(&json!({
    "type": "control_response",
    "response": { "subtype": "success", "request_id": request_id, "response": response },
  }))
}

/// A refusal of one of the program's control requests.
pub fn control_error(request_id: &str, error: &str) -> Vec<u8> {
  line(&json!({
    "type": "control_response",
    "response": { "subtype": "error", "request_id": request_id, "error": error },
  }))
}

/// The answer to a `can_use_tool` request.
pub fn permission_response(request_id: &str, decision: &Decision) -> Vec<u8> {
  let response = match decision {
    Decision::Allow { input, remember } => {
      let mut response = json!({ "behavior": "allow", "updatedInput": input });
      if let Some(remember) = remember {
        response["updatedPermissions"] = remember.clone();
      }
      response
    }
    Decision::Deny { message } => json!({ "behavior": "deny", "message": message }),
  };
  control_response(request_id, response)
}

/// A string field of a JSON object, if it has one.
pub fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
  value.get(key).and_then(Value::as_str)
}

/// The text of a tool result's `content`: a string, or text blocks.
pub fn result_text(content: &Value) -> String {
  match content {
    Value::String(text) => text.clone(),
    Value::Array(blocks) => blocks
      .iter()
      .filter_map(|block| match text(block, "type") {
        Some("text") => text(block, "text").map(str::to_owned),
        Some("image") => Some("[image]".into()),
        _ => None,
      })
      .collect::<Vec<_>>()
      .join("\n"),
    Value::Null => String::new(),
    other => other.to_string(),
  }
}

/// Whether a failed turn's text says the program isn't signed in.
pub fn needs_sign_in(text: &str) -> bool {
  let text = text.to_lowercase();
  [
    "/login",
    "not logged in",
    "invalid api key",
    "please log in",
    "authentication_error",
    "oauth token has expired",
    "credit balance is too low",
  ]
  .iter()
  .any(|phrase| text.contains(phrase))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(bytes: &[u8]) -> Value {
    assert_eq!(bytes.last(), Some(&b'\n'), "one line");
    assert_eq!(bytes.iter().filter(|&&byte| byte == b'\n').count(), 1);
    serde_json::from_slice(bytes).expect("JSON")
  }

  #[test]
  fn arguments_leave_out_empty_options() {
    let plain = arguments(&Start::default());
    assert!(plain.contains(&"--permission-prompt-tool".to_owned()));
    assert!(!plain.contains(&"--model".to_owned()));
    let start = Start {
      model: Some("sonnet".into()),
      mode: Some(" ".into()),
      effort: None,
      resume: Some("abc".into()),
    };
    let args = arguments(&start);
    let after = |flag: &str| {
      args
        .iter()
        .position(|arg| arg == flag)
        .map(|at| args[at + 1].as_str())
    };
    assert_eq!(after("--model"), Some("sonnet"));
    assert_eq!(after("--resume"), Some("abc"));
    assert_eq!(after("--permission-mode"), None, "blank");
  }

  #[test]
  fn messages_are_single_lines_even_with_line_breaks_in_them() {
    let message = parse(&user_message(vec![text_block("two\nlines")]));
    assert_eq!(message["type"], "user");
    assert_eq!(message["message"]["content"][0]["text"], "two\nlines");
    assert_eq!(message["parent_tool_use_id"], Value::Null);
  }

  #[test]
  fn permissions_are_answered_as_the_sdk_does() {
    let input = json!({ "file_path": "a.rs", "content": "x" });
    let remember = json!([{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }]);
    let allowed = parse(&permission_response(
      "r1",
      &Decision::Allow {
        input: input.clone(),
        remember: Some(remember.clone()),
      },
    ));
    assert_eq!(allowed["type"], "control_response");
    assert_eq!(allowed["response"]["subtype"], "success");
    assert_eq!(allowed["response"]["request_id"], "r1");
    assert_eq!(allowed["response"]["response"]["behavior"], "allow");
    assert_eq!(allowed["response"]["response"]["updatedInput"], input);
    assert_eq!(
      allowed["response"]["response"]["updatedPermissions"],
      remember
    );

    let denied = parse(&permission_response(
      "r2",
      &Decision::Deny {
        message: "no".into(),
      },
    ));
    assert_eq!(denied["response"]["response"]["behavior"], "deny");
    assert_eq!(denied["response"]["response"]["message"], "no");
  }

  #[test]
  fn control_requests_carry_their_id() {
    let request = parse(&control_request("init-1", initialize()));
    assert_eq!(request["type"], "control_request");
    assert_eq!(request["request_id"], "init-1");
    assert_eq!(request["request"]["subtype"], "initialize");
  }

  #[test]
  fn tool_results_become_text() {
    assert_eq!(result_text(&json!("plain")), "plain");
    let blocks = json!([{ "type": "text", "text": "a" }, { "type": "image" }, { "type": "text", "text": "b" }]);
    assert_eq!(result_text(&blocks), "a\n[image]\nb");
    assert_eq!(result_text(&Value::Null), "");
  }

  #[test]
  fn sign_in_failures_are_recognized() {
    assert!(needs_sign_in("Invalid API key · Please run /login"));
    assert!(needs_sign_in("Not logged in"));
    assert!(!needs_sign_in("error[E0308]: mismatched types"));
  }
}
