//! One chat: a `claude` program the extension talks to, and the
//! conversation shown in the chat tab's page.
//!
//! The chat turns the program's messages into the transcript's items: the
//! user's messages, Claude's text (rendered as it streams), the tools it
//! uses and what they returned, permission requests (with the diff of a
//! proposed edit, a command to run, a plan to approve, or questions to
//! answer), and notes. It keeps every item, so a page that loads again (its
//! tab was hidden, or the window reloaded) gets the whole conversation.
//!
//! Nothing here calls the host. What the chat has to do goes in its outbox
//! instead: messages for the page, bytes for the program's input, and diff
//! tabs to close. The extension sends them after each call, which keeps
//! the protocol handling testable.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::{Map, Value, json};

use crate::edits::{self, ProposedEdit};
use crate::markdown;
use crate::protocol::{self, Decision, text};

/// Longest tool output kept for the transcript, in characters.
const MAX_OUTPUT_CHARS: usize = 8_000;
/// Longest command or tool input shown, in characters.
const MAX_DETAIL_CHARS: usize = 4_000;
/// Lines of the program's standard error kept, to explain a failure.
const STDERR_LINES: usize = 20;
/// Most of the transcript sent in one page message, in bytes (a message is
/// at most 1 MiB).
const MAX_BATCH_BYTES: usize = 512 * 1024;

/// What the chat needs from the editor while it reads the program's output.
pub trait Files {
  /// A file's text, from the editor when it's open there; `None` when it
  /// doesn't exist or can't be read.
  fn read(&self, path: &str) -> Option<String>;
}

/// Why a chat's program isn't running, with what the user can do about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
  pub text: String,
  /// Buttons: `(label, action)`, the action sent back by the page.
  pub actions: Vec<(String, String)>,
}

/// A permission request waiting for the user.
#[derive(Debug, Clone)]
struct Pending {
  tool: String,
  input: Value,
  suggestions: Option<Value>,
  /// Its proposed edit, by number, for edits.
  edit: Option<u64>,
}

/// Claude's text being streamed, before its message arrives.
#[derive(Debug, Clone)]
struct Stream {
  message: String,
  index: u64,
  item: String,
  text: String,
  done: bool,
}

/// What the chat asks the extension to do.
#[derive(Debug, Default)]
pub struct Outbox {
  /// For the page, in order.
  pub page: Vec<Value>,
  /// For the program's input, in order.
  pub input: Vec<u8>,
  /// Diff tabs of edits that were decided, to close.
  pub close_edits: Vec<String>,
  /// The turn ended (save the chat's history).
  pub turn_ended: bool,
  /// The chat learned its session's id.
  pub session_started: bool,
}

pub struct Chat {
  /// The chat tab's key.
  pub key: String,
  /// The running program, if any.
  pub process: Option<u64>,
  /// The session, once the program said, or the one to resume.
  pub session: Option<String>,
  /// The first line of the first message.
  pub title: Option<String>,
  /// The folder Claude works in, to show paths relative to it.
  pub root: Option<String>,
  /// Why the program isn't running.
  pub problem: Option<Problem>,
  /// The model and permission mode chosen in the chat, if any.
  pub model: Option<String>,
  pub mode: Option<String>,
  /// Whether an Anthropic API key is used (costs are then shown).
  pub api_key: bool,
  /// The editor context the page shows: `{ label, enabled }`.
  pub context: Value,
  /// The program was stopped on purpose (a restart, the tab closing).
  pub stopping: bool,
  pub busy: bool,
  items: Vec<Value>,
  next_item: u64,
  next_request: u64,
  /// The `initialize` answer: models, commands, account.
  init: Option<Value>,
  pending: BTreeMap<String, Pending>,
  streams: Vec<Stream>,
  /// The message being streamed.
  streaming: Option<String>,
  /// Tool items by tool use.
  tools: HashMap<String, String>,
  pub edits: BTreeMap<u64, ProposedEdit>,
  next_edit: u64,
  stderr: VecDeque<String>,
  /// Items changed in this call, sent once at its end.
  dirty: Vec<String>,
  pub outbox: Outbox,
}

/// `text` cut to `max` characters.
fn cut(text: &str, max: usize) -> String {
  match text.char_indices().nth(max) {
    Some((end, _)) => format!("{}…", &text[..end]),
    None => text.to_owned(),
  }
}

fn first_line(text: &str) -> &str {
  text.lines().next().unwrap_or_default()
}

impl Chat {
  pub fn new(key: &str) -> Self {
    Self {
      key: key.to_owned(),
      process: None,
      session: None,
      title: None,
      root: None,
      problem: None,
      model: None,
      mode: None,
      api_key: false,
      context: json!({ "label": null, "enabled": true }),
      stopping: false,
      busy: false,
      items: Vec::new(),
      next_item: 0,
      next_request: 0,
      init: None,
      pending: BTreeMap::new(),
      streams: Vec::new(),
      streaming: None,
      tools: HashMap::new(),
      edits: BTreeMap::new(),
      next_edit: 0,
      stderr: VecDeque::new(),
      dirty: Vec::new(),
      outbox: Outbox::default(),
    }
  }

  // --- The transcript -------------------------------------------------------

  fn new_id(&mut self) -> String {
    self.next_item += 1;
    format!("i{}", self.next_item)
  }

  /// Adds an item (with an `id`), or replaces the one with its id.
  fn put(&mut self, item: Value) {
    let id = text(&item, "id").unwrap_or_default().to_owned();
    match self
      .items
      .iter_mut()
      .find(|existing| text(existing, "id") == Some(id.as_str()))
    {
      Some(existing) => *existing = item,
      None => self.items.push(item),
    }
    if !self.dirty.contains(&id) {
      self.dirty.push(id);
    }
  }

  /// Changes fields of an item.
  fn update(&mut self, id: &str, fields: Value) {
    let Some(item) = self
      .items
      .iter_mut()
      .find(|item| text(item, "id") == Some(id))
    else {
      return;
    };
    if let (Some(item), Value::Object(fields)) = (item.as_object_mut(), fields) {
      item.extend(fields);
    }
    if !self.dirty.iter().any(|dirty| dirty == id) {
      self.dirty.push(id.to_owned());
    }
  }

  fn item(&self, id: &str) -> Option<&Value> {
    self.items.iter().find(|item| text(item, "id") == Some(id))
  }

  /// A note in the transcript: `level` info, warning, or error, with
  /// buttons.
  pub fn note(&mut self, level: &str, message: &str, actions: &[(&str, &str)]) {
    let id = self.new_id();
    let actions: Vec<Value> = actions
      .iter()
      .map(|(label, action)| json!({ "label": label, "action": action }))
      .collect();
    self.put(json!({
      "id": id, "kind": "note", "level": level, "text": message, "actions": actions,
    }));
  }

  /// Sends the page the items changed since the last flush, and the status.
  pub fn flush(&mut self) {
    for id in std::mem::take(&mut self.dirty) {
      if let Some(item) = self.item(&id).cloned() {
        self
          .outbox
          .page
          .push(json!({ "type": "item", "item": item }));
      }
    }
  }

  /// Everything, for a page that loaded: the status, then the transcript in
  /// batches small enough to send.
  pub fn replay(&mut self) {
    self.dirty.clear();
    let mut batches: Vec<Vec<Value>> = vec![Vec::new()];
    let mut size = 0;
    for item in &self.items {
      let bytes = item.to_string().len();
      if size + bytes > MAX_BATCH_BYTES && !batches.last().is_some_and(Vec::is_empty) {
        batches.push(Vec::new());
        size = 0;
      }
      size += bytes;
      if let Some(batch) = batches.last_mut() {
        batch.push(item.clone());
      }
    }
    let status = self.status();
    self
      .outbox
      .page
      .push(json!({ "type": "reset", "status": status }));
    for batch in batches.into_iter().filter(|batch| !batch.is_empty()) {
      self
        .outbox
        .page
        .push(json!({ "type": "items", "items": batch }));
    }
  }

  /// Items kept for the chat's history: the conversation, without tool
  /// output or decided permission requests, the latest that fit in `budget`
  /// bytes.
  pub fn history_items(&self, budget: usize) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::new();
    let mut size = 0;
    for item in self.items.iter().rev() {
      let kept_item = match text(item, "kind") {
        Some("user" | "text" | "result") => item.clone(),
        Some("tool") => json!({
          "id": item["id"], "kind": "tool", "name": item["name"], "title": item["title"],
          "status": item["status"],
        }),
        _ => continue,
      };
      size += kept_item.to_string().len();
      if size > budget {
        break;
      }
      kept.push(kept_item);
    }
    kept.reverse();
    kept
  }

  /// Puts back items from the chat's history, as the conversation so far.
  pub fn restore(&mut self, items: Vec<Value>) {
    for item in items {
      let id = self.new_id();
      let mut item = item;
      item["id"] = Value::String(id);
      self.items.push(item);
    }
  }

  /// Forgets the conversation, for a new session in the same tab.
  pub fn clear(&mut self) {
    self.items.clear();
    self.pending.clear();
    self.streams.clear();
    self.tools.clear();
    self.dirty.clear();
    self.title = None;
    self.session = None;
  }

  // --- Status ---------------------------------------------------------------

  /// What the page shows besides the transcript.
  pub fn status(&self) -> Value {
    let init = self.init.as_ref();
    let models: Vec<Value> = init
      .and_then(|init| init.get("models"))
      .and_then(Value::as_array)
      .map(|models| {
        models
          .iter()
          .filter_map(|model| {
            Some(json!({
              "value": text(model, "value")?,
              "label": text(model, "displayName").unwrap_or(text(model, "value")?),
              "description": text(model, "description").unwrap_or_default(),
            }))
          })
          .collect()
      })
      .unwrap_or_default();
    let commands: Vec<Value> = init
      .and_then(|init| init.get("commands"))
      .and_then(Value::as_array)
      .map(|commands| {
        commands
          .iter()
          .filter_map(|command| {
            Some(json!({
              "name": text(command, "name")?,
              "description": cut(text(command, "description").unwrap_or_default(), 120),
              "hint": text(command, "argumentHint").unwrap_or_default(),
            }))
          })
          .collect()
      })
      .unwrap_or_default();
    let account = init
      .and_then(|init| init.get("account"))
      .filter(|account| !account.is_null())
      .map(|account| {
        text(account, "email")
          .or(text(account, "organization"))
          .unwrap_or("signed in")
          .to_owned()
      });
    let mode = self
      .mode
      .clone()
      .or_else(|| init.and_then(|init| text(init, "current_permission_mode").map(str::to_owned)))
      .unwrap_or_else(|| "default".into());
    json!({
      "type": "status",
      "busy": self.busy,
      "running": self.process.is_some(),
      "models": models,
      "model": self.model.clone().unwrap_or_else(|| "default".into()),
      "mode": mode,
      "account": account,
      "title": self.title,
      "commands": commands,
      "problem": self.problem.as_ref().map(|problem| json!({
        "text": problem.text,
        "actions": problem.actions.iter().map(|(label, action)| json!({ "label": label, "action": action })).collect::<Vec<_>>(),
      })),
      "context": self.context,
    })
  }

  pub fn post_status(&mut self) {
    let status = self.status();
    self.outbox.page.push(status);
  }

  // --- Talking to the program -----------------------------------------------

  fn request_id(&mut self, kind: &str) -> String {
    self.next_request += 1;
    format!("{kind}-{}", self.next_request)
  }

  /// The bytes to start a program with: its `initialize` request.
  pub fn first_input(&mut self) -> Vec<u8> {
    let id = self.request_id("init");
    protocol::control_request(&id, protocol::initialize())
  }

  /// The program started.
  pub fn started(&mut self, process: u64) {
    self.process = Some(process);
    self.problem = None;
    self.stopping = false;
    self.stderr.clear();
    self.post_status();
  }

  /// The user sent a message: `text` as they wrote it, and `context`
  /// blocks before it (what the editor shows, the label naming it).
  pub fn send(&mut self, message: &str, context: Vec<String>, label: Option<String>) {
    if self.title.is_none() {
      let title = cut(first_line(message.trim()), 60);
      if !title.is_empty() {
        self.title = Some(title);
      }
    }
    let id = self.new_id();
    self.put(json!({ "id": id, "kind": "user", "text": message, "context": label }));
    let mut blocks: Vec<Value> = context
      .iter()
      .map(|text| protocol::text_block(text))
      .collect();
    blocks.push(protocol::text_block(message));
    self.outbox.input.extend(protocol::user_message(blocks));
    self.busy = true;
    self.post_status();
  }

  pub fn interrupt(&mut self) {
    if self.process.is_some() {
      let id = self.request_id("interrupt");
      self
        .outbox
        .input
        .extend(protocol::control_request(&id, protocol::interrupt()));
    }
  }

  pub fn set_model(&mut self, model: &str) {
    self.model = Some(model.to_owned());
    if self.process.is_some() {
      let id = self.request_id("model");
      self
        .outbox
        .input
        .extend(protocol::control_request(&id, protocol::set_model(model)));
    }
    self.post_status();
  }

  pub fn set_mode(&mut self, mode: &str) {
    self.mode = Some(mode.to_owned());
    if self.process.is_some() {
      let id = self.request_id("mode");
      self.outbox.input.extend(protocol::control_request(
        &id,
        protocol::set_permission_mode(mode),
      ));
    }
    self.post_status();
  }

  /// Lines the program wrote.
  pub fn output(&mut self, lines: Vec<(bool, String)>, files: &dyn Files) {
    for (stderr, line) in lines {
      if stderr {
        if self.stderr.len() == STDERR_LINES {
          self.stderr.pop_front();
        }
        self.stderr.push_back(line);
        continue;
      }
      let line = line.trim();
      if line.is_empty() {
        continue;
      }
      match serde_json::from_str::<Value>(line) {
        Ok(message) => self.handle(&message, files),
        // Not the protocol: something it printed, worth keeping for errors.
        Err(_) => {
          if self.stderr.len() == STDERR_LINES {
            self.stderr.pop_front();
          }
          self.stderr.push_back(line.to_owned());
        }
      }
    }
    self.flush();
  }

  /// The program ended: `failure` says why, if it failed to run.
  pub fn ended(&mut self, exit_code: Option<i32>, failure: Option<String>) {
    self.process = None;
    self.busy = false;
    for (request, _) in std::mem::take(&mut self.pending) {
      self.resolve_item(&request, "cancelled");
    }
    for stream in &mut self.streams {
      stream.done = true;
    }
    if !std::mem::take(&mut self.stopping) {
      let tail: Vec<&str> = self.stderr.iter().map(String::as_str).collect();
      let tail = tail.join("\n");
      let why = failure.unwrap_or_else(|| match exit_code {
        Some(0) => String::new(),
        Some(code) => format!("It exited with code {code}."),
        None => "It was stopped.".into(),
      });
      if protocol::needs_sign_in(&tail) {
        self.sign_in_note(&tail);
      } else if !why.is_empty() || !tail.is_empty() {
        let message = if tail.is_empty() {
          format!("Claude Code stopped. {why}")
        } else {
          format!("Claude Code stopped. {why}\n\n{}", cut(&tail, 2_000))
        };
        self.note("error", message.trim(), &[("Start Again", "restart")]);
      }
    }
    self.flush();
    self.post_status();
  }

  fn sign_in_note(&mut self, message: &str) {
    self.note(
      "warning",
      &format!(
        "Claude Code isn't signed in. {}",
        cut(first_line(message), 300)
      ),
      &[
        ("Sign In", "signIn"),
        ("Use an API Key", "apiKey"),
        ("Try Again", "restart"),
      ],
    );
  }

  /// One message from the program.
  pub fn handle(&mut self, message: &Value, files: &dyn Files) {
    match text(message, "type") {
      Some("control_response") => self.control_response(&message["response"]),
      Some("control_request") => self.control_request(message, files),
      Some("control_cancel_request") => {
        if let Some(request) = text(message, "request_id")
          && self.pending.remove(request).is_some()
        {
          self.resolve_item(request, "cancelled");
        }
      }
      Some("system") => self.system(message),
      Some("stream_event") if message["parent_tool_use_id"].is_null() => {
        self.stream_event(&message["event"]);
      }
      Some("assistant") if message["parent_tool_use_id"].is_null() => {
        self.assistant(&message["message"]);
      }
      Some("user") if message["parent_tool_use_id"].is_null() => self.user(&message["message"]),
      Some("result") => self.result(message),
      Some("rate_limit_event") => {
        let info = &message["rate_limit_info"];
        if text(info, "status")
          .is_some_and(|status| status != "allowed" && status != "allowed_warning")
        {
          let id = self.new_id();
          self.put(json!({
            "id": id, "kind": "note", "level": "warning",
            "text": "You've reached your Claude usage limit.",
            "resetsAt": info["resetsAt"], "actions": [],
          }));
        }
      }
      _ => {}
    }
  }

  fn control_response(&mut self, response: &Value) {
    let id = text(response, "request_id").unwrap_or_default();
    if text(response, "subtype") == Some("error") {
      let error = text(response, "error").unwrap_or("it failed");
      if id.starts_with("model") {
        self.note(
          "error",
          &format!("The model couldn't be changed: {error}"),
          &[],
        );
      } else if id.starts_with("mode") {
        self.note(
          "error",
          &format!("The mode couldn't be changed: {error}"),
          &[],
        );
      } else if !id.starts_with("interrupt") {
        self.note("error", &format!("Claude Code: {error}"), &[]);
      }
      return;
    }
    if id.starts_with("init") {
      self.init = Some(response["response"].clone());
      self.post_status();
    }
  }

  fn control_request(&mut self, message: &Value, files: &dyn Files) {
    let id = text(message, "request_id").unwrap_or_default().to_owned();
    let request = &message["request"];
    if text(request, "subtype") != Some("can_use_tool") {
      // Hooks and SDK tools aren't used: say so rather than leave it waiting.
      self.outbox.input.extend(protocol::control_error(
        &id,
        "Flareite doesn't handle this request",
      ));
      return;
    }
    let tool = text(request, "tool_name").unwrap_or("a tool").to_owned();
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let suggestions = request
      .get("permission_suggestions")
      .filter(|suggestions| suggestions.as_array().is_some_and(|list| !list.is_empty()))
      .cloned();
    let mut item = Map::new();
    item.insert("id".into(), json!(id));
    item.insert("kind".into(), json!("permission"));
    item.insert("tool".into(), json!(tool));
    item.insert("title".into(), json!(self.tool_title(&tool, &input)));
    item.insert("state".into(), json!("pending"));
    item.insert("allowSession".into(), json!(suggestions.is_some()));
    if let Some(reason) = text(request, "decision_reason").or(text(request, "blocked_path")) {
      item.insert("reason".into(), json!(cut(reason, 500)));
    }
    let mut edit = None;
    match tool.as_str() {
      "AskUserQuestion" => {
        item.insert("questions".into(), input["questions"].clone());
      }
      "ExitPlanMode" => {
        let plan = text(&input, "plan").unwrap_or_default();
        item.insert("plan".into(), json!(markdown::to_html(plan)));
      }
      _ => {
        if let Some(path) = edits::edited_file(&tool, &input) {
          let current = files.read(path);
          if let Some((original, proposed, approximate)) =
            edits::texts(&tool, &input, current.as_deref())
          {
            let changes = edits::compare(&original, &proposed);
            let mut diff = edits::hunks(&original, &proposed, &changes);
            self.next_edit += 1;
            let number = self.next_edit;
            diff["edit"] = json!(format!("{}.{number}", self.key));
            diff["path"] = json!(self.relative(path));
            diff["approximate"] = json!(approximate);
            diff["created"] = json!(current.is_none() && tool == "Write");
            item.insert("diff".into(), diff);
            self.edits.insert(
              number,
              ProposedEdit {
                request: id.clone(),
                path: path.to_owned(),
                original,
                proposed,
                approximate,
                changes,
              },
            );
            edit = Some(number);
          }
        } else if let Some(command) = text(&input, "command") {
          item.insert("command".into(), json!(cut(command, MAX_DETAIL_CHARS)));
          if let Some(description) = text(&input, "description") {
            item.insert("description".into(), json!(cut(description, 300)));
          }
        } else if !input.is_null() {
          item.insert(
            "input".into(),
            json!(cut(
              &serde_json::to_string_pretty(&input).unwrap_or_default(),
              MAX_DETAIL_CHARS
            )),
          );
        }
      }
    }
    self.pending.insert(
      id,
      Pending {
        tool,
        input,
        suggestions,
        edit,
      },
    );
    self.put(Value::Object(item));
  }

  /// The user decided a permission request: `decision` is `allow`,
  /// `allowSession`, or `deny`; `answers` answers a question, and
  /// `message` says why it's denied.
  pub fn decide(
    &mut self,
    request: &str,
    decision: &str,
    answers: Option<Value>,
    message: Option<String>,
  ) {
    let Some(pending) = self.pending.remove(request) else {
      return;
    };
    let decision = match decision {
      "allow" | "allowSession" => {
        let mut input = pending.input.clone();
        if let Some(answers) = answers {
          input["answers"] = answers;
        }
        let remember = if decision == "allowSession" {
          pending.suggestions.clone().or_else(|| {
            // An edit with nothing suggested: edits are accepted from now on.
            pending.edit.map(
              |_| json!([{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }]),
            )
          })
        } else {
          None
        };
        Decision::Allow { input, remember }
      }
      _ => Decision::Deny {
        message: match message.filter(|message| !message.trim().is_empty()) {
          Some(message) => format!("The user denied this, saying: {message}"),
          None => format!(
            "The user denied this {} request. Ask them what to do instead.",
            pending.tool
          ),
        },
      },
    };
    self
      .outbox
      .input
      .extend(protocol::permission_response(request, &decision));
    let state = match decision {
      Decision::Allow { .. } => "allowed",
      Decision::Deny { .. } => "denied",
    };
    self.resolve_item(request, state);
    if let Some(number) = pending.edit {
      self
        .outbox
        .close_edits
        .push(format!("{}.{number}", self.key));
    }
    self.flush();
  }

  /// The permission request an edit is for, if it's still waiting.
  pub fn edit_request(&self, number: u64) -> Option<String> {
    let edit = self.edits.get(&number)?;
    self
      .pending
      .contains_key(&edit.request)
      .then(|| edit.request.clone())
  }

  fn resolve_item(&mut self, request: &str, state: &str) {
    self.update(request, json!({ "state": state }));
    // Its edit's texts aren't needed once decided.
    let decided: Vec<u64> = self
      .edits
      .iter()
      .filter(|(_, edit)| edit.request == request)
      .map(|(number, _)| *number)
      .collect();
    for number in decided {
      if state == "cancelled" {
        self
          .outbox
          .close_edits
          .push(format!("{}.{number}", self.key));
      }
      self.edits.remove(&number);
    }
  }

  fn system(&mut self, message: &Value) {
    match text(message, "subtype") {
      Some("init") => {
        if let Some(session) = text(message, "session_id")
          && self.session.as_deref() != Some(session)
        {
          self.session = Some(session.to_owned());
          self.outbox.session_started = true;
        }
        if let Some(mode) = text(message, "permissionMode") {
          self.mode = Some(mode.to_owned());
        }
        self.post_status();
      }
      Some("compact_boundary") => self.note("info", "The conversation was compacted.", &[]),
      _ => {}
    }
  }

  fn stream_event(&mut self, event: &Value) {
    match text(event, "type") {
      Some("message_start") => {
        self.streaming = text(&event["message"], "id").map(str::to_owned);
      }
      Some("content_block_start") if text(&event["content_block"], "type") == Some("text") => {
        let Some(message) = self.streaming.clone() else {
          return;
        };
        let index = event["index"].as_u64().unwrap_or(0);
        let item = self.new_id();
        self.streams.push(Stream {
          message,
          index,
          item: item.clone(),
          text: String::new(),
          done: false,
        });
        self.put(json!({ "id": item, "kind": "text", "html": "", "streaming": true }));
      }
      Some("content_block_delta") if text(&event["delta"], "type") == Some("text_delta") => {
        let index = event["index"].as_u64().unwrap_or(0);
        let Some(message) = self.streaming.clone() else {
          return;
        };
        let delta = text(&event["delta"], "text").unwrap_or_default();
        let Some(stream) = self
          .streams
          .iter_mut()
          .find(|stream| stream.message == message && stream.index == index && !stream.done)
        else {
          return;
        };
        stream.text.push_str(delta);
        let (item, html) = (stream.item.clone(), markdown::to_html(&stream.text));
        self.update(&item, json!({ "html": html }));
      }
      _ => {}
    }
  }

  fn assistant(&mut self, message: &Value) {
    let id = text(message, "id").unwrap_or_default().to_owned();
    let Some(content) = message["content"].as_array() else {
      return;
    };
    for block in content {
      match text(block, "type") {
        Some("text") => {
          let body = text(block, "text").unwrap_or_default();
          if body.trim().is_empty() {
            continue;
          }
          let html = markdown::to_html(body);
          // The text it streamed, now whole.
          let streamed = self
            .streams
            .iter_mut()
            .find(|stream| stream.message == id && !stream.done);
          match streamed {
            Some(stream) => {
              stream.done = true;
              let item = stream.item.clone();
              self.update(&item, json!({ "html": html, "streaming": false }));
            }
            None => {
              let item = self.new_id();
              self.put(json!({ "id": item, "kind": "text", "html": html, "streaming": false }));
            }
          }
        }
        Some("thinking") => {
          let thinking = text(block, "thinking").unwrap_or_default();
          if !thinking.trim().is_empty() {
            let item = self.new_id();
            self.put(json!({
              "id": item, "kind": "thinking", "text": cut(thinking, MAX_OUTPUT_CHARS),
            }));
          }
        }
        Some("tool_use") => {
          let use_id = text(block, "id").unwrap_or_default().to_owned();
          let name = text(block, "name").unwrap_or("Tool").to_owned();
          let input = &block["input"];
          let item = self.new_id();
          let title = self.tool_title(&name, input);
          let detail = self.tool_detail(&name, input);
          self.tools.insert(use_id, item.clone());
          self.put(json!({
            "id": item, "kind": "tool", "name": name, "title": title, "detail": detail,
            "status": "running",
          }));
        }
        _ => {}
      }
    }
  }

  fn user(&mut self, message: &Value) {
    match &message["content"] {
      // A slash command's output.
      Value::String(content) => {
        if let Some(output) = between(content, "<local-command-stdout>", "</local-command-stdout>")
          .or_else(|| between(content, "<local-command-stderr>", "</local-command-stderr>"))
        {
          let output = output.trim();
          if !output.is_empty() {
            let id = self.new_id();
            self.put(json!({
              "id": id, "kind": "note", "level": "info", "text": cut(output, MAX_OUTPUT_CHARS),
              "preformatted": true, "actions": [],
            }));
          }
        }
      }
      Value::Array(blocks) => {
        for block in blocks {
          if text(block, "type") != Some("tool_result") {
            continue;
          }
          let Some(item) = text(block, "tool_use_id")
            .and_then(|id| self.tools.get(id))
            .cloned()
          else {
            continue;
          };
          let failed = block["is_error"].as_bool().unwrap_or(false);
          let output = protocol::result_text(&block["content"]);
          self.update(
            &item,
            json!({
              "status": if failed { "error" } else { "done" },
              "output": cut(output.trim_end(), MAX_OUTPUT_CHARS),
            }),
          );
        }
      }
      _ => {}
    }
  }

  fn result(&mut self, message: &Value) {
    self.busy = false;
    for stream in &mut self.streams {
      stream.done = true;
    }
    // Tools still running were interrupted.
    let running: Vec<String> = self
      .items
      .iter()
      .filter(|item| text(item, "kind") == Some("tool") && text(item, "status") == Some("running"))
      .filter_map(|item| text(item, "id").map(str::to_owned))
      .collect();
    for id in running {
      self.update(&id, json!({ "status": "stopped" }));
    }
    let failed = message["is_error"].as_bool().unwrap_or(false)
      || text(message, "subtype").is_some_and(|subtype| subtype != "success");
    if failed {
      let reason = text(message, "result")
        .map(str::to_owned)
        .or_else(|| {
          message["errors"].as_array().map(|errors| {
            errors
              .iter()
              .filter_map(Value::as_str)
              .collect::<Vec<_>>()
              .join("\n")
          })
        })
        .unwrap_or_else(|| text(message, "subtype").unwrap_or("error").to_owned());
      if protocol::needs_sign_in(&reason) {
        self.sign_in_note(&reason);
      } else if text(message, "subtype") == Some("error_during_execution") && reason.is_empty() {
        self.note("info", "Stopped.", &[]);
      } else {
        self.note("error", &cut(&reason, 2_000), &[]);
      }
    } else {
      let seconds = message["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0;
      let mut summary = format!("{seconds:.1}s");
      if self.api_key
        && let Some(cost) = message["total_cost_usd"].as_f64()
      {
        summary.push_str(&format!(" · ${cost:.4}"));
      }
      let id = self.new_id();
      self.put(json!({ "id": id, "kind": "result", "text": summary }));
    }
    self.outbox.turn_ended = true;
    self.post_status();
  }

  // --- Describing tools ---------------------------------------------------------

  /// A path as it's shown: relative to the folder when inside it.
  pub fn relative(&self, path: &str) -> String {
    if let Some(root) = &self.root {
      let normalize = |path: &str| path.replace('\\', "/").to_lowercase();
      let (folder, full) = (normalize(root), normalize(path));
      let folder = folder.trim_end_matches('/');
      if full.starts_with(folder) && full[folder.len()..].starts_with('/') {
        return path[folder.len() + 1..].replace('\\', "/");
      }
    }
    path.to_owned()
  }

  /// What a tool use does, in a few words.
  fn tool_title(&self, name: &str, input: &Value) -> String {
    let path = |key: &str| text(input, key).map(|path| self.relative(path));
    let quoted = |key: &str| text(input, key).map(|value| cut(first_line(value), 80));
    let title = match name {
      "Read" => path("file_path").map(|path| format!("Read {path}")),
      "Write" => path("file_path").map(|path| format!("Write {path}")),
      "Edit" | "MultiEdit" => path("file_path").map(|path| format!("Edit {path}")),
      "NotebookEdit" => path("notebook_path").map(|path| format!("Edit {path}")),
      "Bash" | "PowerShell" => quoted("command").map(|command| format!("Run {command}")),
      "Glob" => quoted("pattern").map(|pattern| format!("Find {pattern}")),
      "Grep" => quoted("pattern").map(|pattern| format!("Search for {pattern}")),
      "WebFetch" => quoted("url").map(|url| format!("Fetch {url}")),
      "WebSearch" => quoted("query").map(|query| format!("Search the web for {query}")),
      "Task" | "Agent" => quoted("description").map(|task| format!("Agent: {task}")),
      "TodoWrite" => Some("Update the to-do list".into()),
      "AskUserQuestion" => Some("A question for you".into()),
      "ExitPlanMode" => Some("Claude's plan".into()),
      _ => None,
    };
    title.unwrap_or_else(|| name.to_owned())
  }

  /// More about a tool use, shown when its row is opened.
  fn tool_detail(&self, name: &str, input: &Value) -> Value {
    match name {
      "Bash" | "PowerShell" => json!(cut(
        text(input, "command").unwrap_or_default(),
        MAX_DETAIL_CHARS
      )),
      "TodoWrite" => json!(
        input["todos"]
          .as_array()
          .map(|todos| {
            todos
              .iter()
              .map(|todo| {
                let mark = match text(todo, "status") {
                  Some("completed") => "☑",
                  Some("in_progress") => "◐",
                  _ => "☐",
                };
                format!("{mark} {}", text(todo, "content").unwrap_or_default())
              })
              .collect::<Vec<_>>()
              .join("\n")
          })
          .unwrap_or_default()
      ),
      "Read" | "Write" | "Edit" | "MultiEdit" | "Glob" | "Grep" => Value::Null,
      _ if input.as_object().is_some_and(|input| !input.is_empty()) => json!(cut(
        &serde_json::to_string_pretty(input).unwrap_or_default(),
        MAX_DETAIL_CHARS
      )),
      _ => Value::Null,
    }
  }
}

/// The text between `start` and `end` in `text`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
  let from = text.find(start)? + start.len();
  let to = text[from..].find(end).map_or(text.len(), |at| from + at);
  Some(&text[from..to])
}

#[cfg(test)]
mod tests {
  use super::*;

  struct NoFiles;
  impl Files for NoFiles {
    fn read(&self, _: &str) -> Option<String> {
      None
    }
  }

  struct OneFile(&'static str, &'static str);
  impl Files for OneFile {
    fn read(&self, path: &str) -> Option<String> {
      (path == self.0).then(|| self.1.to_owned())
    }
  }

  fn lines(messages: &[Value]) -> Vec<(bool, String)> {
    messages
      .iter()
      .map(|message| (false, message.to_string()))
      .collect()
  }

  fn written(chat: &mut Chat) -> Vec<Value> {
    let input = std::mem::take(&mut chat.outbox.input);
    String::from_utf8(input)
      .expect("UTF-8")
      .lines()
      .map(|line| serde_json::from_str(line).expect("JSON"))
      .collect()
  }

  fn posted_items(chat: &mut Chat) -> Vec<Value> {
    std::mem::take(&mut chat.outbox.page)
      .into_iter()
      .filter(|message| message["type"] == "item")
      .map(|message| message["item"].clone())
      .collect()
  }

  #[test]
  fn streamed_text_becomes_one_item_finished_by_its_message() {
    let mut chat = Chat::new("1");
    let stream =
      |event: Value| json!({ "type": "stream_event", "event": event, "parent_tool_use_id": null });
    chat.output(
      lines(&[
        stream(json!({ "type": "message_start", "message": { "id": "m1" } })),
        stream(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
        stream(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "**Hel" } })),
        stream(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "lo**" } })),
      ]),
      &NoFiles,
    );
    let items = posted_items(&mut chat);
    assert_eq!(items.len(), 1, "changes in one call are sent once");
    assert_eq!(items[0]["html"], "<p><strong>Hello</strong></p>\n");
    assert_eq!(items[0]["streaming"], true);
    chat.output(
      lines(&[json!({
        "type": "assistant", "parent_tool_use_id": null,
        "message": { "id": "m1", "content": [{ "type": "text", "text": "**Hello**" }] },
      })]),
      &NoFiles,
    );
    let items = posted_items(&mut chat);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "i1", "the same item");
    assert_eq!(items[0]["streaming"], false);
  }

  #[test]
  fn tools_show_their_results() {
    let mut chat = Chat::new("1");
    chat.root = Some("C:\\work".into());
    chat.output(
      lines(&[
        json!({
          "type": "assistant", "parent_tool_use_id": null,
          "message": { "id": "m1", "content": [{
            "type": "tool_use", "id": "t1", "name": "Read", "input": { "file_path": "C:\\work\\src\\main.rs" },
          }] },
        }),
        json!({
          "type": "user", "parent_tool_use_id": null,
          "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}" }] },
        }),
      ]),
      &NoFiles,
    );
    let items = posted_items(&mut chat);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "Read src/main.rs");
    assert_eq!(items[0]["status"], "done");
    assert_eq!(items[0]["output"], "fn main() {}");
  }

  #[test]
  fn edits_ask_with_a_diff_and_are_answered() {
    let mut chat = Chat::new("7");
    chat.output(
      lines(&[json!({
        "type": "control_request", "request_id": "r1",
        "request": {
          "subtype": "can_use_tool", "tool_name": "Edit",
          "input": { "file_path": "/w/a.txt", "old_string": "two", "new_string": "2" },
          "permission_suggestions": [{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }],
        },
      })]),
      &OneFile("/w/a.txt", "one\ntwo\nthree\n"),
    );
    let items = posted_items(&mut chat);
    assert_eq!(items[0]["kind"], "permission");
    assert_eq!(items[0]["state"], "pending");
    assert_eq!(items[0]["diff"]["edit"], "7.1");
    assert_eq!(items[0]["diff"]["added"], 1);
    assert_eq!(chat.edit_request(1).as_deref(), Some("r1"));
    assert_eq!(chat.edits[&1].proposed, "one\n2\nthree\n");

    chat.decide("r1", "allowSession", None, None);
    let answer = written(&mut chat);
    assert_eq!(answer[0]["response"]["request_id"], "r1");
    assert_eq!(answer[0]["response"]["response"]["behavior"], "allow");
    assert_eq!(
      answer[0]["response"]["response"]["updatedPermissions"][0]["mode"],
      "acceptEdits"
    );
    assert_eq!(chat.outbox.close_edits, ["7.1"]);
    assert!(chat.edits.is_empty(), "forgotten once decided");
    assert_eq!(posted_items(&mut chat)[0]["state"], "allowed");
    // Deciding again does nothing.
    chat.decide("r1", "deny", None, None);
    assert!(chat.outbox.input.is_empty());
  }

  #[test]
  fn questions_are_answered_in_the_input() {
    let mut chat = Chat::new("1");
    let questions = json!([{ "question": "Which?", "header": "Pick", "multiSelect": false,
      "options": [{ "label": "A", "description": "" }, { "label": "B", "description": "" }] }]);
    chat.output(
      lines(&[json!({
        "type": "control_request", "request_id": "q1",
        "request": { "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": { "questions": questions } },
      })]),
      &NoFiles,
    );
    assert_eq!(posted_items(&mut chat)[0]["questions"], questions);
    chat.decide("q1", "allow", Some(json!({ "Which?": "B" })), None);
    let answer = written(&mut chat);
    let input = &answer[0]["response"]["response"]["updatedInput"];
    assert_eq!(input["answers"]["Which?"], "B");
    assert_eq!(input["questions"], questions);
  }

  #[test]
  fn denials_tell_claude_why() {
    let mut chat = Chat::new("1");
    chat.output(
      lines(&[json!({
        "type": "control_request", "request_id": "b1",
        "request": { "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "rm -rf build" } },
      })]),
      &NoFiles,
    );
    assert_eq!(posted_items(&mut chat)[0]["command"], "rm -rf build");
    chat.decide("b1", "deny", None, Some("use cargo clean".into()));
    let answer = written(&mut chat);
    assert_eq!(answer[0]["response"]["response"]["behavior"], "deny");
    assert!(
      answer[0]["response"]["response"]["message"]
        .as_str()
        .unwrap()
        .contains("use cargo clean")
    );
  }

  #[test]
  fn other_control_requests_are_refused_not_left_waiting() {
    let mut chat = Chat::new("1");
    chat.output(
      lines(&[json!({
        "type": "control_request", "request_id": "h1", "request": { "subtype": "hook_callback" },
      })]),
      &NoFiles,
    );
    let answer = written(&mut chat);
    assert_eq!(answer[0]["response"]["subtype"], "error");
    assert_eq!(answer[0]["response"]["request_id"], "h1");
  }

  #[test]
  fn the_initialize_answer_fills_the_status() {
    let mut chat = Chat::new("1");
    let input = chat.first_input();
    let request: Value = serde_json::from_slice(&input).expect("JSON");
    let id = request["request_id"].as_str().unwrap().to_owned();
    chat.output(
      lines(&[json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": id, "response": {
          "models": [{ "value": "default", "displayName": "Default", "description": "Opus" }],
          "commands": [{ "name": "compact", "description": "Compact", "argumentHint": "" }],
          "account": { "email": "a@example.com" },
          "current_permission_mode": "plan",
        } },
      })]),
      &NoFiles,
    );
    let status = chat.status();
    assert_eq!(status["models"][0]["label"], "Default");
    assert_eq!(status["commands"][0]["name"], "compact");
    assert_eq!(status["account"], "a@example.com");
    assert_eq!(status["mode"], "plan");
  }

  #[test]
  fn results_end_the_turn_and_sign_in_failures_say_so() {
    let mut chat = Chat::new("1");
    chat.send("hi", Vec::new(), None);
    assert!(chat.busy);
    let sent = written(&mut chat);
    assert_eq!(sent[0]["message"]["content"][0]["text"], "hi");
    chat.output(
      lines(&[json!({
        "type": "result", "subtype": "success", "is_error": true,
        "result": "Invalid API key · Please run /login",
      })]),
      &NoFiles,
    );
    assert!(!chat.busy);
    assert!(chat.outbox.turn_ended);
    let items = posted_items(&mut chat);
    let note = items
      .iter()
      .find(|item| item["kind"] == "note")
      .expect("a note");
    assert!(note["text"].as_str().unwrap().contains("isn't signed in"));
    assert_eq!(note["actions"][0]["action"], "signIn");
  }

  #[test]
  fn sessions_are_learned_from_the_init_message() {
    let mut chat = Chat::new("1");
    chat.output(
      lines(&[json!({ "type": "system", "subtype": "init", "session_id": "s1", "permissionMode": "default" })]),
      &NoFiles,
    );
    assert_eq!(chat.session.as_deref(), Some("s1"));
    assert!(chat.outbox.session_started);
  }

  #[test]
  fn a_failed_program_explains_itself_unless_stopped_on_purpose() {
    let mut chat = Chat::new("1");
    chat.started(3);
    chat.output(vec![(true, "boom".into())], &NoFiles);
    chat.ended(Some(1), None);
    let items = posted_items(&mut chat);
    assert!(
      items[0]["text"].as_str().unwrap().contains("boom"),
      "{items:?}"
    );
    assert_eq!(chat.process, None);

    chat.started(4);
    chat.stopping = true;
    chat.ended(None, None);
    assert!(posted_items(&mut chat).is_empty());
  }

  #[test]
  fn history_keeps_the_conversation_within_a_budget() {
    let mut chat = Chat::new("1");
    chat.send("first", Vec::new(), None);
    chat.send(&"x".repeat(1_000), Vec::new(), None);
    let kept = chat.history_items(600);
    assert!(kept.is_empty(), "the latest doesn't fit: {kept:?}");
    let kept = chat.history_items(10_000);
    assert_eq!(kept.len(), 2);
    let mut other = Chat::new("2");
    other.restore(kept);
    other.replay();
    let batches: Vec<&Value> = other
      .outbox
      .page
      .iter()
      .filter(|message| message["type"] == "items")
      .collect();
    assert_eq!(batches[0]["items"][0]["text"], "first");
  }

  #[test]
  fn paths_inside_the_folder_are_relative() {
    let mut chat = Chat::new("1");
    chat.root = Some("C:\\Work\\app".into());
    assert_eq!(chat.relative("c:\\work\\app\\src\\a.rs"), "src/a.rs");
    assert_eq!(
      chat.relative("C:\\Work\\application\\a.rs"),
      "C:\\Work\\application\\a.rs"
    );
    chat.root = Some("/home/me/app/".into());
    assert_eq!(chat.relative("/home/me/app/a.rs"), "a.rs");
  }

  #[test]
  fn slash_command_output_is_shown() {
    let mut chat = Chat::new("1");
    chat.output(
      lines(&[json!({
        "type": "user", "parent_tool_use_id": null,
        "message": { "content": "<local-command-stdout>Total cost: $0.01</local-command-stdout>" },
      })]),
      &NoFiles,
    );
    let items = posted_items(&mut chat);
    assert_eq!(items[0]["text"], "Total cost: $0.01");
    assert_eq!(items[0]["preformatted"], true);
  }
}
