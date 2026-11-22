//! Claude Code for Flareite: chats with Claude Code, Anthropic's coding
//! agent, in tabs beside the editor.
//!
//! Each chat tab (the `chat` layout, one webview pane) has a `claude`
//! program of its own, started with `processes::spawn-interactive` (API
//! 2.1) and spoken to in its JSON stream protocol (`protocol`): the user's
//! messages go to its input, and what it writes comes back as
//! `process-output` events, which the chat (`chat`) turns into the
//! conversation the page shows. Claude asks before it edits a file or runs
//! a command; the chat shows what it would do, a proposed edit as a diff,
//! which also opens in an `edit` layout tab (`edits`).
//!
//! The extension also sends the editor's context along with a message (the
//! file in use and its selection), offers the open folder's files for
//! @-mentions and the program's slash commands, lists past chats to resume
//! (`history`), and runs `claude` in a terminal to sign in.

mod chat;
mod edits;
mod history;
mod markdown;
mod protocol;

use std::collections::{BTreeMap, HashMap};

use flareite_extension_api::events::{Event, SelectionChange};
use flareite_extension_api::layouts::{
  self, FillReason, FillRequest, Layout, LayoutNode, LayoutTab, NodeKind, Pane, PaneContent,
  PaneMessage,
};
use flareite_extension_api::processes::{self, ProcessError, ProcessOptions};
use flareite_extension_api::prompts::{self, AnswerValue, ConfirmOptions, InputOptions, PickItem};
use flareite_extension_api::terminals::{self, TerminalOptions};
use flareite_extension_api::types::{
  Activation, CommandContext, ConfigValue, LogLevel, MessageLevel, Position, Range, Severity,
  StatusItem,
};
use flareite_extension_api::{
  Extension, files, host, links, register, secrets, workbench, workspace,
};
use serde_json::{Value, json};

use chat::{Chat, Problem};
use protocol::text;

const CHAT: &str = "chat";
/// The chat tab's one pane.
const PAGE: &str = "page";
/// The secret holding the user's Anthropic API key.
const API_KEY: &str = "anthropic-api-key";
/// Where to install Claude Code from.
const INSTALL_URL: &str = "https://code.claude.com/docs/en/setup";
/// Most of a selection sent along with a message, in bytes.
const MAX_SELECTION_BYTES: usize = 32 * 1024;
/// Most problems put in a message.
const MAX_PROBLEMS: usize = 50;

/// The permission mode that never asks.
const BYPASS: &str = "bypassPermissions";
/// The setting that lets chats use it.
const ALLOW_BYPASS: &str = "allow_bypass_permissions";

fn log(level: LogLevel, message: &str) {
  host::log(level, message);
}

fn error(message: &str) {
  host::show_message(MessageLevel::Error, message);
}

fn setting(key: &str) -> Option<String> {
  match host::configuration(key) {
    Some(ConfigValue::Text(value)) if !value.trim().is_empty() => Some(value),
    _ => None,
  }
}

fn flag(key: &str, default: bool) -> bool {
  match host::configuration(key) {
    Some(ConfigValue::Boolean(value)) => value,
    _ => default,
  }
}

/// Saves a choice from a chat's menus as the setting new chats start with;
/// `None` goes back to the setting's default.
fn remember(key: &str, value: Option<&str>) {
  if setting(key).as_deref() == value {
    return;
  }
  let value = value.map(|value| ConfigValue::Text(value.to_owned()));
  if let Err(problem) = host::set_configuration(key, value.as_ref()) {
    log(LogLevel::Warn, &format!("couldn't save {key}: {problem:?}"));
  }
}

fn chat_tab(key: &str) -> LayoutTab {
  LayoutTab {
    layout: CHAT.into(),
    key: key.into(),
  }
}

/// The folder's path, if one is open.
fn root() -> Option<String> {
  workspace::folder().map(|folder| folder.path)
}

/// Paths compare as the same file: separators and, on Windows, case.
fn same_path(a: &str, b: &str) -> bool {
  let normalize = |path: &str| {
    let path = path.replace('\\', "/");
    if path.chars().nth(1) == Some(':') {
      path.to_lowercase()
    } else {
      path
    }
  };
  normalize(a) == normalize(b)
}

/// Reads files for the chat: the folder's, as the editor has them.
struct EditorFiles;

impl chat::Files for EditorFiles {
  fn read(&self, path: &str) -> Option<String> {
    files::read_file(path).ok()
  }
}

/// What a prompt the extension showed is for.
enum Asked {
  ApiKey,
  /// Whether to allow Bypass permissions, chosen in this chat.
  Bypass(String),
  /// A past chat to resume, in a new tab.
  Resume,
}

/// The editor's context for a chat: the file in use and its selection.
struct Context {
  /// The file's path, and the document's id when it's a text document.
  file: Option<String>,
  document: Option<u64>,
  /// Lines selected, from 0, end exclusive; `None` without a selection.
  lines: Option<(u32, u32)>,
  selection: Option<Range>,
}

struct ClaudeCode {
  chats: BTreeMap<String, Chat>,
  /// Chats by their running program.
  processes: HashMap<u64, String>,
  /// The chat used last.
  last_chat: Option<String>,
  /// The file in use, and the selection in the document in use.
  active_file: Option<String>,
  selection: Option<SelectionChange>,
  prompts: HashMap<u64, Asked>,
  /// A file to show at a line once it's open.
  reveal: Option<(String, u32)>,
  /// Text to put in a chat's message box once its page loads.
  inserts: HashMap<String, String>,
  /// When each chat was last used, by the page's clock.
  used: HashMap<String, f64>,
}

impl Extension for ClaudeCode {
  fn activate(_: Activation) -> Result<Self, String> {
    let extension = Self {
      chats: BTreeMap::new(),
      processes: HashMap::new(),
      last_chat: None,
      active_file: workbench::active_file(),
      selection: None,
      prompts: HashMap::new(),
      reveal: None,
      inserts: HashMap::new(),
      used: HashMap::new(),
    };
    extension.update_status_item();
    Ok(extension)
  }

  fn deactivate(&mut self) {
    for chat in self.chats.values_mut() {
      if let Some(process) = chat.process {
        chat.stopping = true;
        processes::kill(process);
      }
    }
  }

  fn run_command(&mut self, command: &str, context: CommandContext) -> Result<(), String> {
    match command.trim_start_matches("flareite.claude-code.") {
      "open" => self.open(),
      "newChat" => self.new_chat(),
      "addSelection" => {
        let mention = self
          .selection_mention()
          .ok_or("Select some text in a file first.")?;
        self.insert(&format!("{mention} "));
      }
      "addFile" => {
        let path = context.argument.ok_or("Choose a file in the explorer.")?;
        let shown = self.relative(&path);
        self.insert(&format!("@{shown} "));
      }
      "fixProblems" => {
        let document = context
          .document
          .ok_or("Open the file whose problems Claude should fix.")?;
        let text = problems_text(document, &|path| self.relative(path))
          .ok_or("This file has no problems.")?;
        self.insert(&text);
      }
      "resume" => self.pick_session()?,
      "runInTerminal" => self.run_in_terminal(&[], "Claude Code")?,
      "signIn" => self.run_in_terminal(&["auth".into(), "login".into()], "Claude Code: Sign In")?,
      "setApiKey" => self.ask_api_key()?,
      "clearApiKey" => {
        secrets::set(API_KEY, None).map_err(|error| format!("{error:?}"))?;
        host::show_message(
          MessageLevel::Info,
          "Claude Code will use its own sign-in from the next chat.",
        );
      }
      "acceptEdit" | "acceptAllEdits" | "rejectEdit" => {
        let edit = context.argument.ok_or("No edit is named.")?;
        let decision = match command.trim_start_matches("flareite.claude-code.") {
          "acceptEdit" => "allow",
          "acceptAllEdits" => "allowSession",
          _ => "deny",
        };
        self.decide_edit(&edit, decision);
      }
      other => return Err(format!("unknown command {other}")),
    }
    Ok(())
  }

  fn handle_event(&mut self, event: Event) -> Result<(), String> {
    match event {
      Event::LayoutFill(request) => self.fill(request),
      Event::PaneMessage(message) => self.page_message(message),
      Event::ProcessOutput(output) => {
        let Some(key) = self.processes.get(&output.process).cloned() else {
          return Ok(());
        };
        if output.dropped > 0 {
          log(LogLevel::Warn, "lines of Claude Code's output were dropped");
        }
        if let Some(chat) = self.chats.get_mut(&key) {
          let lines = output
            .lines
            .into_iter()
            .map(|line| (line.stderr, line.text))
            .collect();
          chat.output(lines, &EditorFiles);
        }
        self.after(&key);
      }
      Event::ProcessFinished(finished) => {
        let Some(key) = self.processes.remove(&finished.process) else {
          return Ok(());
        };
        if let Some(chat) = self.chats.get_mut(&key) {
          match finished.outcome {
            Ok(output) => chat.ended(output.exit_code, None),
            Err(ProcessError::Killed) => chat.ended(None, None),
            Err(error) => chat.ended(None, Some(process_problem(&error))),
          }
        }
        self.after(&key);
      }
      Event::ActiveFileChanged(file) => {
        self.active_file = file;
        self.context_changed();
      }
      Event::SelectionChanged(change) => {
        self.selection = Some(change);
        self.context_changed();
      }
      Event::FolderChanged => {
        // A folder opened, or was trusted: chats waiting for it start.
        let waiting: Vec<String> = self
          .chats
          .iter()
          .filter(|(_, chat)| chat.process.is_none() && chat.problem.is_some())
          .map(|(key, _)| key.clone())
          .collect();
        for key in waiting {
          self.start(&key);
        }
      }
      Event::PromptAnswered(answer) => {
        let Some(asked) = self.prompts.remove(&answer.prompt) else {
          return Ok(());
        };
        match (asked, answer.value) {
          (Asked::ApiKey, AnswerValue::Text(key)) if !key.trim().is_empty() => {
            secrets::set(API_KEY, Some(key.trim())).map_err(|error| format!("{error:?}"))?;
            self.restart_idle_chats();
          }
          (Asked::Resume, AnswerValue::Picked(session)) => self.resume_in_new_tab(&session),
          (Asked::Bypass(key), AnswerValue::Button(0)) => {
            host::set_configuration(ALLOW_BYPASS, Some(&ConfigValue::Boolean(true)))
              .map_err(|error| format!("{error:?}"))?;
            self.set_mode(&key, BYPASS);
            self.after(&key);
          }
          _ => {}
        }
      }
      Event::DocumentOpened(info) => {
        if let (Some((path, line)), Some(opened)) = (self.reveal.clone(), info.path.as_deref())
          && same_path(&path, opened)
        {
          self.reveal = None;
          reveal_line(info.id, line);
        }
      }
      _ => {}
    }
    Ok(())
  }

  fn provide_text(&mut self, uri: &str) -> Result<Option<String>, String> {
    let Some((edit, side)) = edits::parse_address(uri) else {
      return Ok(None);
    };
    Ok(self.edit(edit).map(|edit| {
      if side == "original" {
        edit.original.clone()
      } else {
        edit.proposed.clone()
      }
    }))
  }
}

impl ClaudeCode {
  // --- Chat tabs ------------------------------------------------------------

  /// Shows the chat used last, or a new one.
  fn open(&mut self) {
    let open: Vec<String> = layouts::tabs()
      .into_iter()
      .filter(|tab| tab.layout == CHAT)
      .map(|tab| tab.key)
      .collect();
    let key = self
      .last_chat
      .clone()
      .filter(|key| open.contains(key))
      .or_else(|| open.last().cloned());
    match key {
      Some(key) => self.show_chat(&key),
      None => self.new_chat(),
    }
  }

  fn new_chat(&mut self) {
    let open: Vec<String> = layouts::tabs()
      .into_iter()
      .filter(|tab| tab.layout == CHAT)
      .map(|tab| tab.key)
      .collect();
    let key = history::next_chat(&open);
    self.chats.insert(key.clone(), self.new_chat_state(&key));
    self.show_chat(&key);
  }

  fn new_chat_state(&self, key: &str) -> Chat {
    let mut chat = Chat::new(key);
    chat.root = root();
    chat.api_key = matches!(secrets::get(API_KEY), Ok(Some(_)));
    // The menus start where they were left.
    chat.model = setting("model");
    chat.mode = setting("permission_mode");
    chat.effort = setting("effort");
    chat.context = self.context_status();
    chat
  }

  fn show_chat(&mut self, key: &str) {
    self.last_chat = Some(key.to_owned());
    if let Err(problem) = layouts::open(&chat_tab(key), flag("open_beside", true)) {
      error(&format!("Opening the chat failed: {problem:?}"));
    }
  }

  /// Fills in a tab of one of the layouts.
  fn fill(&mut self, request: FillRequest) {
    let key = request.tab.key.clone();
    if request.tab.layout == edits::LAYOUT {
      match request.reason {
        FillReason::Opened | FillReason::Restored => match self.edit(&key).cloned() {
          Some(edit) => edits::fill(&key, &edit),
          // From an earlier start: the request is gone.
          None => layouts::close(&request.tab),
        },
        FillReason::Ready => {
          if let Some(edit) = self.edit(&key).cloned() {
            edits::decorate(&key, &edit);
          }
        }
        _ => {}
      }
      return;
    }
    if request.tab.layout != CHAT {
      return;
    }
    match request.reason {
      FillReason::Opened | FillReason::Restored => {
        if !self.chats.contains_key(&key) {
          let mut chat = self.new_chat_state(&key);
          // A tab from an earlier session continues its conversation.
          if let Some(session) = history::tab(&key) {
            chat.restore(history::items(&session));
            chat.title = history::title(&session);
            chat.session = Some(session);
          }
          self.chats.insert(key.clone(), chat);
        }
        self.set_chat_layout(&key);
      }
      FillReason::Closed => {
        if let Some(mut chat) = self.chats.remove(&key) {
          if let Some(process) = chat.process.take() {
            self.processes.remove(&process);
            processes::kill(process);
          }
          for number in chat.edits.keys() {
            layouts::close(&edits::tab(&format!("{key}.{number}")));
          }
        }
        history::set_tab(&key, None);
        self.update_status_item();
      }
      _ => {}
    }
  }

  fn set_chat_layout(&self, key: &str) {
    let title = self
      .chats
      .get(key)
      .and_then(|chat| chat.title.clone())
      .map_or_else(|| "Claude Code".to_owned(), |title| format!("✻ {title}"));
    let layout = Layout {
      title,
      nodes: vec![LayoutNode {
        id: PAGE.into(),
        parent: None,
        size: 1.0,
        kind: NodeKind::Pane(Pane {
          content: PaneContent::Webview(Some("media/chat.html".into())),
          header: None,
          scroll_group: None,
        }),
      }],
      documents: Vec::new(),
      buttons: Vec::new(),
      breakpoints: Vec::new(),
      main: Some(PAGE.into()),
    };
    if let Err(problem) = layouts::set_layout(&chat_tab(key), &layout) {
      log(
        LogLevel::Warn,
        &format!("can't show chat {key}: {problem:?}"),
      );
    }
  }

  /// Does what a chat asked for after a call: messages for its page, input
  /// for its program, diff tabs to close, history to save.
  fn after(&mut self, key: &str) {
    let Some(chat) = self.chats.get_mut(key) else {
      return;
    };
    chat.flush();
    let outbox = std::mem::take(&mut chat.outbox);
    let process = chat.process;
    let session = chat.session.clone();
    let title = chat.title.clone();
    let history =
      (outbox.turn_ended && session.is_some()).then(|| chat.history_items(history::ITEMS_BUDGET));
    let retitle = outbox.session_started || outbox.turn_ended;
    for message in outbox.page {
      let _ = layouts::post_message(&chat_tab(key), PAGE, &message.to_string());
    }
    if !outbox.input.is_empty()
      && let Some(process) = process
      && let Err(problem) = processes::write_input(process, &outbox.input)
    {
      log(
        LogLevel::Warn,
        &format!("couldn't write to Claude Code: {problem:?}"),
      );
    }
    for edit in outbox.close_edits {
      layouts::close(&edits::tab(&edit));
    }
    if outbox.session_started {
      history::set_tab(key, session.as_deref());
    }
    if let (Some(session), Some(items)) = (session, history) {
      let updated = self.used.get(key).copied().unwrap_or(0.0);
      history::save(
        &session,
        title.as_deref().unwrap_or("Untitled chat"),
        updated,
        items,
      );
    }
    if retitle {
      self.set_chat_layout(key);
    }
    // Bypass permissions was chosen during the turn.
    if outbox.turn_ended
      && self
        .chats
        .get(key)
        .is_some_and(|chat| chat.restart_after_turn)
    {
      self.restart(key);
    }
    self.update_status_item();
  }

  // --- Starting the program ---------------------------------------------------

  /// Starts a chat's program, unless it runs: continuing its session, if
  /// it has one. Says in the chat why when it can't.
  fn start(&mut self, key: &str) {
    let Some(chat) = self.chats.get_mut(key) else {
      return;
    };
    if chat.process.is_some() {
      return;
    }
    chat.root = root();
    let problem = match workspace::folder() {
      None => Some(Problem {
        text: "Open a folder for Claude Code to work in.".into(),
        actions: vec![("Open Folder…".into(), "openFolder".into())],
      }),
      Some(folder) if !folder.trusted => Some(Problem {
        text: "Claude Code runs programs in the folder it works in, so it runs only in a folder you trust.".into(),
        actions: vec![("Trust the Folder…".into(), "trust".into())],
      }),
      Some(_) => None,
    };
    if let Some(problem) = problem {
      chat.problem = Some(problem);
      chat.post_status();
      self.after(key);
      return;
    }
    let api_key = secrets::get(API_KEY).ok().flatten();
    chat.api_key = api_key.is_some();
    let allow_bypass = flag(ALLOW_BYPASS, false);
    if !allow_bypass && chat.mode.as_deref() == Some(BYPASS) {
      chat.mode = None;
    }
    chat.bypass_allowed = allow_bypass;
    chat.restart_after_turn = false;
    let start = protocol::Start {
      model: chat.model.clone().filter(|model| model != "default"),
      mode: chat.mode.clone(),
      effort: chat.effort.clone(),
      resume: chat.session.clone(),
      allow_bypass,
    };
    let mut env = Vec::new();
    if let Some(api_key) = api_key {
      env.push(("ANTHROPIC_API_KEY".to_owned(), api_key));
    }
    let options = ProcessOptions {
      cwd: None,
      env,
      stdin: Some(chat.first_input()),
      timeout_ms: None,
    };
    match processes::spawn_interactive("claude", &protocol::arguments(&start), &options) {
      Ok(process) => {
        log(
          LogLevel::Info,
          &format!("started Claude Code for chat {key}"),
        );
        chat.started(process);
        self.processes.insert(process, key.to_owned());
      }
      Err(problem) => {
        chat.problem = Some(match problem {
          ProcessError::NotFound(_) => Problem {
            text: "Claude Code isn't installed, or isn't on the PATH. Install it, then try again. (Or set where it is in settings.toml, under [extensions.programs] as \"flareite.claude-code.claude\".)".into(),
            actions: vec![
              ("How to Install".into(), "install".into()),
              ("Try Again".into(), "restart".into()),
            ],
          },
          ProcessError::PermissionDenied => Problem {
            text: "Claude Code needs permission to run programs: allow it in the Extensions view.".into(),
            actions: vec![("Try Again".into(), "restart".into())],
          },
          ProcessError::Untrusted => Problem {
            text: "Claude Code runs only in a folder you trust.".into(),
            actions: vec![("Trust the Folder…".into(), "trust".into())],
          },
          other => Problem {
            text: format!("Claude Code couldn't start: {}", process_problem(&other)),
            actions: vec![("Try Again".into(), "restart".into())],
          },
        });
        chat.post_status();
      }
    }
    self.after(key);
  }

  /// Stops a chat's program and starts it again (continuing its session).
  fn restart(&mut self, key: &str) {
    if let Some(chat) = self.chats.get_mut(key)
      && let Some(process) = chat.process
    {
      chat.stopping = true;
      processes::kill(process);
      // Its end is still reported; the new one is known by its own id.
      self.processes.remove(&process);
      chat.ended(None, None);
    }
    self.start(key);
  }

  /// The user chose a permission mode in a chat's menu. Bypass permissions
  /// asks first, the first time.
  fn choose_mode(&mut self, key: &str, mode: &str) {
    if mode == BYPASS && !flag(ALLOW_BYPASS, false) {
      // The menu shows the mode in use until the user agrees.
      if let Some(chat) = self.chats.get_mut(key) {
        chat.post_status();
      }
      match prompts::confirm(&ConfirmOptions {
        message: "Let Claude Code run commands and change files without asking?".into(),
        detail: Some(
          "In Bypass permissions mode, Claude doesn't ask before it runs a command or changes a file. Its commands aren't limited to this folder: they can do anything you can on this computer, such as deleting files or using the internet. Choose another mode in the menu to make it ask again.".into(),
        ),
        buttons: vec!["Allow Bypass Permissions".into()],
        destructive: true,
      }) {
        Ok(prompt) => {
          self.prompts.insert(prompt, Asked::Bypass(key.to_owned()));
        }
        Err(problem) => log(
          LogLevel::Warn,
          &format!("couldn't ask about Bypass permissions: {problem:?}"),
        ),
      }
      return;
    }
    self.set_mode(key, mode);
  }

  fn set_mode(&mut self, key: &str, mode: &str) {
    remember(
      "permission_mode",
      Some(mode).filter(|mode| *mode != "default"),
    );
    let Some(chat) = self.chats.get_mut(key) else {
      return;
    };
    if mode != BYPASS || chat.process.is_none() || chat.bypass_allowed {
      chat.set_mode(mode);
      return;
    }
    // The program has to start again, able to bypass, continuing the session.
    chat.mode = Some(mode.to_owned());
    if chat.busy {
      chat.restart_after_turn = true;
      chat.note(
        "info",
        "Bypass permissions starts when Claude finishes this turn.",
        &[],
      );
      chat.post_status();
    } else {
      self.restart(key);
    }
  }

  /// Restarts chats that aren't in the middle of a turn: after the API key
  /// changed.
  fn restart_idle_chats(&mut self) {
    let idle: Vec<String> = self
      .chats
      .iter()
      .filter(|(_, chat)| chat.process.is_some() && !chat.busy)
      .map(|(key, _)| key.clone())
      .collect();
    for key in idle {
      self.restart(&key);
    }
  }

  // --- The page -------------------------------------------------------------

  fn page_message(&mut self, message: PaneMessage) {
    if message.tab.layout != CHAT {
      return;
    }
    let key = message.tab.key;
    let Ok(data) = serde_json::from_str::<Value>(&message.data) else {
      return;
    };
    if !self.chats.contains_key(&key) {
      return;
    }
    self.last_chat = Some(key.clone());
    match text(&data, "type").unwrap_or_default() {
      "ready" => {
        let context = self.context_status();
        if let Some(chat) = self.chats.get_mut(&key) {
          chat.context = context;
          chat.replay();
        }
        if let Some(insert) = self.inserts.remove(&key) {
          self.post(&key, json!({ "type": "insert", "text": insert }));
        }
        let idle = self
          .chats
          .get(&key)
          .is_some_and(|chat| chat.process.is_none() && chat.problem.is_none());
        if idle {
          self.start(&key);
        }
      }
      "send" => self.send(&key, &data),
      "interrupt" => {
        if let Some(chat) = self.chats.get_mut(&key) {
          chat.interrupt();
        }
      }
      "permission" => {
        if let Some(chat) = self.chats.get_mut(&key) {
          chat.decide(
            text(&data, "id").unwrap_or_default(),
            text(&data, "decision").unwrap_or("deny"),
            data
              .get("answers")
              .cloned()
              .filter(|answers| !answers.is_null()),
            text(&data, "message").map(str::to_owned),
          );
        }
      }
      "openDiff" => {
        if let Some(edit) = text(&data, "edit")
          && let Err(problem) = edits::open(edit)
        {
          error(&format!("Showing the edit failed: {problem}"));
        }
      }
      "openFile" => {
        if let Some(path) = text(&data, "path") {
          let line = data["line"]
            .as_u64()
            .map(|line| u32::try_from(line).unwrap_or(0));
          self.open_file(path, line);
        }
      }
      "openLink" => {
        if let Some(url) = text(&data, "url")
          && let Err(problem) = links::open_in_browser(url)
        {
          log(LogLevel::Warn, &format!("couldn't open {url}: {problem:?}"));
        }
      }
      "model" => {
        if let (Some(chat), Some(model)) = (self.chats.get_mut(&key), text(&data, "value")) {
          chat.set_model(model);
          remember("model", Some(model).filter(|model| *model != "default"));
        }
      }
      "mode" => {
        if let Some(mode) = text(&data, "value") {
          self.choose_mode(&key, mode);
        }
      }
      "effort" => {
        if let (Some(chat), Some(level)) = (self.chats.get_mut(&key), text(&data, "value")) {
          chat.set_effort(level);
          remember("effort", chat.effort.as_deref());
        }
      }
      "newChat" => self.new_chat(),
      "history" => {
        let sessions = history::sessions();
        self.post(&key, json!({ "type": "history", "sessions": sessions }));
      }
      "resume" => {
        if let Some(session) = text(&data, "id") {
          self.resume(&key, session);
        }
      }
      "files" => {
        let files = self.folder_files();
        self.post(&key, json!({ "type": "files", "files": files }));
      }
      "problems" => {
        let document = self.context().document;
        let text =
          document.and_then(|document| problems_text(document, &|path| self.relative(path)));
        match text {
          Some(text) => self.post(&key, json!({ "type": "insert", "text": text })),
          None => self.post(
            &key,
            json!({ "type": "toast", "text": "The file in use has no problems." }),
          ),
        }
      }
      "action" => self.action(&key, text(&data, "name").unwrap_or_default()),
      _ => {}
    }
    self.after(&key);
  }

  fn post(&self, key: &str, message: Value) {
    let _ = layouts::post_message(&chat_tab(key), PAGE, &message.to_string());
  }

  /// A button of a note or problem in the chat.
  fn action(&mut self, key: &str, name: &str) {
    let outcome = match name {
      "restart" => {
        if let Some(chat) = self.chats.get_mut(key) {
          chat.problem = None;
        }
        self.restart(key);
        Ok(())
      }
      "signIn" => self.run_in_terminal(&["auth".into(), "login".into()], "Claude Code: Sign In"),
      "apiKey" => self.ask_api_key(),
      "trust" => {
        workspace::request_trust();
        Ok(())
      }
      "openFolder" => prompts::pick_folder("Open a Folder for Claude Code")
        .map(|_| ())
        .map_err(|problem| format!("{problem:?}")),
      "install" => links::open_in_browser(INSTALL_URL).map_err(|problem| format!("{problem:?}")),
      _ => Ok(()),
    };
    if let Err(problem) = outcome {
      error(&problem);
    }
  }

  /// The user sent a message from the page.
  fn send(&mut self, key: &str, data: &Value) {
    let message = text(data, "text").unwrap_or_default().trim_end().to_owned();
    if message.trim().is_empty() {
      return;
    }
    if let Some(now) = data["now"].as_f64() {
      self.used.insert(key.to_owned(), now);
    }
    let with_context = data["context"].as_bool().unwrap_or(false);
    let (blocks, label) = if with_context {
      self.context_blocks()
    } else {
      (Vec::new(), None)
    };
    let running = self
      .chats
      .get(key)
      .is_some_and(|chat| chat.process.is_some());
    if !running {
      if let Some(chat) = self.chats.get_mut(key) {
        chat.problem = None;
      }
      self.start(key);
    }
    let Some(chat) = self.chats.get_mut(key) else {
      return;
    };
    if chat.process.is_none() {
      chat.note(
        "error",
        "The message wasn't sent: Claude Code isn't running.",
        &[],
      );
      return;
    }
    chat.send(&message, blocks, label);
  }

  /// Resumes a past session in a chat tab, in place of its conversation.
  fn resume(&mut self, key: &str, session: &str) {
    if let Some(chat) = self.chats.get_mut(key) {
      if let Some(process) = chat.process.take() {
        chat.stopping = true;
        self.processes.remove(&process);
        processes::kill(process);
      }
      chat.busy = false;
      chat.clear();
      chat.restore(history::items(session));
      chat.title = history::title(session);
      chat.session = Some(session.to_owned());
      chat.note("info", "Continuing this chat.", &[]);
      chat.replay();
    }
    history::set_tab(key, Some(session));
    self.start(key);
    self.set_chat_layout(key);
  }

  fn resume_in_new_tab(&mut self, session: &str) {
    let open: Vec<String> = layouts::tabs()
      .into_iter()
      .filter(|tab| tab.layout == CHAT)
      .map(|tab| tab.key)
      .collect();
    let key = history::next_chat(&open);
    let mut chat = self.new_chat_state(&key);
    chat.restore(history::items(session));
    chat.title = history::title(session);
    chat.session = Some(session.to_owned());
    self.chats.insert(key.clone(), chat);
    history::set_tab(&key, Some(session));
    self.show_chat(&key);
  }

  fn pick_session(&mut self) -> Result<(), String> {
    let sessions = history::sessions();
    if sessions.is_empty() {
      return Err("There are no past chats in this folder.".into());
    }
    let items: Vec<PickItem> = sessions
      .iter()
      .filter_map(|session| {
        Some(PickItem {
          label: text(session, "title").unwrap_or("Untitled chat").to_owned(),
          description: None,
          value: text(session, "id")?.to_owned(),
        })
      })
      .collect();
    let prompt =
      prompts::pick("Resume a past chat", &items).map_err(|problem| format!("{problem:?}"))?;
    self.prompts.insert(prompt, Asked::Resume);
    Ok(())
  }

  /// Puts text in the message box of the chat used last (opening one),
  /// and shows it.
  fn insert(&mut self, text: &str) {
    let open: Vec<String> = layouts::tabs()
      .into_iter()
      .filter(|tab| tab.layout == CHAT)
      .map(|tab| tab.key)
      .collect();
    let key = self.last_chat.clone().filter(|key| open.contains(key));
    match key {
      Some(key) if self.chats.contains_key(&key) => {
        self.post(&key, json!({ "type": "insert", "text": text }));
        self.show_chat(&key);
      }
      _ => {
        self.new_chat();
        if let Some(key) = self.last_chat.clone() {
          self.inserts.insert(key, text.to_owned());
        }
      }
    }
  }

  // --- The editor -----------------------------------------------------------

  fn relative(&self, path: &str) -> String {
    let mut chat = Chat::new("");
    chat.root = root();
    chat.relative(path)
  }

  /// The open folder's files, relative to it, for @-mentions.
  fn folder_files(&self) -> Vec<String> {
    let Some(root) = root() else {
      return Vec::new();
    };
    files::list_files(&root)
      .map(|list| list.paths.iter().map(|path| self.relative(path)).collect())
      .unwrap_or_default()
  }

  fn context(&self) -> Context {
    let mut context = Context {
      file: self.active_file.clone(),
      document: None,
      lines: None,
      selection: None,
    };
    let Some(change) = &self.selection else {
      return context;
    };
    let Some(document) = change.document else {
      return context;
    };
    let Ok(info) = host::document(document) else {
      return context;
    };
    // The selection is the file in use's only if it's that document.
    if let (Some(active), Some(path)) = (&context.file, &info.path)
      && !same_path(active, path)
    {
      return context;
    }
    context.file = info.path.or(context.file);
    context.document = Some(document);
    if let Some(range) = change.selections.first().copied()
      && range.start != range.end
    {
      let end = if range.end.column == 0 && range.end.line > range.start.line {
        range.end.line
      } else {
        range.end.line + 1
      };
      context.lines = Some((range.start.line, end));
      context.selection = Some(range);
    }
    context
  }

  /// The editor context as the page shows it: `{ label, enabled }`.
  fn context_status(&self) -> Value {
    let context = self.context();
    let label = context.file.as_deref().map(|file| {
      let name = edits::file_name(file);
      match context.lines {
        Some((start, end)) if end > start + 1 => format!("{name}:{}-{end}", start + 1),
        Some((start, _)) => format!("{name}:{}", start + 1),
        None => name.to_owned(),
      }
    });
    json!({ "label": label, "enabled": flag("include_editor_context", true) })
  }

  fn context_changed(&mut self) {
    let status = self.context_status();
    let keys: Vec<String> = self.chats.keys().cloned().collect();
    for key in keys {
      if let Some(chat) = self.chats.get_mut(&key) {
        chat.context = status.clone();
      }
      self.post(&key, json!({ "type": "context", "context": status }));
    }
  }

  /// What's sent along with a message about the editor: the file in use,
  /// and the selection's text; and the label naming it.
  fn context_blocks(&self) -> (Vec<String>, Option<String>) {
    let context = self.context();
    let Some(file) = &context.file else {
      return (Vec::new(), None);
    };
    let shown = self.relative(file);
    let label = self.context_status()["label"].as_str().map(str::to_owned);
    let selected = context
      .document
      .zip(context.selection)
      .and_then(|(document, range)| host::read_range(document, range).ok())
      .map(|slice| slice.text);
    let block = match (selected, context.lines) {
      (Some(selected), Some((start, end))) => {
        let mut selected = selected;
        if selected.len() > MAX_SELECTION_BYTES {
          let mut cut = MAX_SELECTION_BYTES;
          while !selected.is_char_boundary(cut) {
            cut -= 1;
          }
          selected.truncate(cut);
          selected.push_str("\n…");
        }
        let language = file.rsplit('.').next().unwrap_or_default();
        format!(
          "<editor-context>\nIn Flareite, the user has {shown} open, with lines {}–{end} selected:\n```{language}\n{selected}\n```\n</editor-context>",
          start + 1
        )
      }
      _ => format!("<editor-context>\nIn Flareite, the user has {shown} open.\n</editor-context>"),
    };
    (vec![block], label)
  }

  /// `@path#L3-9` for the selection in use.
  fn selection_mention(&self) -> Option<String> {
    let context = self.context();
    let file = self.relative(context.file.as_deref()?);
    Some(match context.lines {
      Some((start, end)) if end > start + 1 => format!("@{file}#L{}-{end}", start + 1),
      Some((start, _)) => format!("@{file}#L{}", start + 1),
      None => format!("@{file}"),
    })
  }

  /// Opens a file the chat names (relative to the folder), at a line.
  fn open_file(&mut self, path: &str, line: Option<u32>) {
    let absolute = match root() {
      Some(root) if !std::path::Path::new(path).is_absolute() && !path.starts_with('/') => {
        format!("{}/{}", root.trim_end_matches(['/', '\\']), path)
      }
      _ => path.to_owned(),
    };
    if let Err(problem) = workspace::open_file(&absolute) {
      log(
        LogLevel::Info,
        &format!("couldn't open {absolute}: {problem:?}"),
      );
      return;
    }
    let Some(line) = line else {
      return;
    };
    let open = host::documents().ok().and_then(|documents| {
      documents.into_iter().find(|document| {
        document
          .path
          .as_deref()
          .is_some_and(|open| same_path(open, &absolute))
      })
    });
    match open {
      Some(document) => reveal_line(document.id, line),
      None => self.reveal = Some((absolute, line)),
    }
  }

  // --- Terminals and sign-in -----------------------------------------------

  fn run_in_terminal(&self, args: &[String], title: &str) -> Result<(), String> {
    let mut env = Vec::new();
    if let Ok(Some(api_key)) = secrets::get(API_KEY) {
      env.push(("ANTHROPIC_API_KEY".to_owned(), api_key));
    }
    terminals::run(
      "claude",
      args,
      &TerminalOptions {
        title: Some(title.into()),
        cwd: None,
        env,
      },
    )
    .map_err(|problem| match problem {
      ProcessError::NotFound(_) => {
        format!("Claude Code isn't installed, or isn't on the PATH. See {INSTALL_URL}")
      }
      other => format!("Running Claude Code failed: {}", process_problem(&other)),
    })
  }

  fn ask_api_key(&mut self) -> Result<(), String> {
    let prompt = prompts::input_password(&InputOptions {
      title: "Your Anthropic API key, kept in the system's credential store".into(),
      value: String::new(),
      placeholder: Some("sk-ant-…".into()),
    })
    .map_err(|problem| format!("{problem:?}"))?;
    self.prompts.insert(prompt, Asked::ApiKey);
    Ok(())
  }

  // --- Proposed edits -------------------------------------------------------

  /// An edit by its id, `<chat>.<number>`.
  fn edit(&self, id: &str) -> Option<&edits::ProposedEdit> {
    let (key, number) = id.rsplit_once('.')?;
    self.chats.get(key)?.edits.get(&number.parse().ok()?)
  }

  fn decide_edit(&mut self, id: &str, decision: &str) {
    let Some((key, number)) = id.rsplit_once('.') else {
      return;
    };
    let Some(number) = number.parse::<u64>().ok() else {
      return;
    };
    let key = key.to_owned();
    if let Some(chat) = self.chats.get_mut(&key) {
      match chat.edit_request(number) {
        Some(request) => {
          let message = (decision == "deny").then(|| "The user rejected this edit.".to_owned());
          chat.decide(&request, decision, None, message);
        }
        None => layouts::close(&edits::tab(id)),
      }
    }
    self.after(&key);
  }

  // --- The status bar ---------------------------------------------------------

  fn update_status_item(&self) {
    let busy = self.chats.values().filter(|chat| chat.busy).count();
    let item = StatusItem {
      text: if busy > 0 {
        "✻ Claude…".into()
      } else {
        "✻ Claude".into()
      },
      tooltip: Some(if busy > 0 {
        "Claude is working. Open Claude Code (Ctrl+Esc)".into()
      } else {
        "Open Claude Code (Ctrl+Esc)".into()
      }),
      command: Some("flareite.claude-code.open".into()),
    };
    let _ = workbench::set_status_item("claude", Some(&item));
  }
}

/// Selects a line of an open document and shows it.
fn reveal_line(document: u64, line: u32) {
  let at = Position {
    line: line.saturating_sub(1),
    column: 0,
  };
  let _ = workbench::reveal(document, Range { start: at, end: at });
}

/// Why a program couldn't run, in words.
fn process_problem(problem: &ProcessError) -> String {
  match problem {
    ProcessError::NotFound(why)
    | ProcessError::TooLarge(why)
    | ProcessError::InvalidArgument(why)
    | ProcessError::Failed(why) => why.clone(),
    ProcessError::UnknownProgram(name) => format!("{name} isn't declared"),
    ProcessError::PermissionDenied => "the permission to run programs isn't granted".into(),
    ProcessError::Untrusted => "the folder isn't trusted".into(),
    ProcessError::TimedOut => "it took too long".into(),
    ProcessError::Killed => "it was stopped".into(),
  }
}

/// A message asking Claude to fix a document's problems, listing them.
fn problems_text(document: u64, relative: &dyn Fn(&str) -> String) -> Option<String> {
  let info = host::document(document).ok()?;
  let problems = host::diagnostics(document).ok()?;
  let listed: Vec<String> = problems
    .iter()
    .filter(|problem| problem.severity != Severity::Hint)
    .take(MAX_PROBLEMS)
    .map(|problem| {
      let severity = match problem.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Information => "info",
        Severity::Hint => "hint",
      };
      let source = problem
        .source
        .as_deref()
        .map(|source| format!(" ({source})"))
        .unwrap_or_default();
      format!(
        "- line {}: {severity}: {}{source}",
        problem.range.start.line + 1,
        problem.message.lines().next().unwrap_or_default()
      )
    })
    .collect();
  if listed.is_empty() {
    return None;
  }
  let file = info
    .path
    .as_deref()
    .map_or_else(|| "this file".to_owned(), relative);
  Some(format!(
    "Fix these problems in {file}:\n{}\n",
    listed.join("\n")
  ))
}

register!(ClaudeCode);
