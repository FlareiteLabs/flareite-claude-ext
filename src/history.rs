//! Past chats, kept in the open folder's storage so they can be resumed.
//!
//! Claude Code keeps every session itself, and `--resume` continues one;
//! what the extension keeps is what the chat tab shows: each session's
//! title, when it was last used (the page's clock: extensions have none),
//! and a short copy of the conversation to show again. It also remembers
//! which session each open chat tab holds, for tabs restored with the
//! window. A folder's storage holds 1 MiB, so at most [`MAX_SESSIONS`]
//! sessions are kept, each within [`ITEMS_BUDGET`].

use flareite_extension_api::storage::{self, StorageScope};
use serde_json::{Map, Value, json};

/// The sessions, most recently used first: `[{ id, title, updated }]`.
const INDEX: &str = "sessions";
/// Open chat tabs' sessions, by tab key.
const TABS: &str = "tabs";
/// Where a session's conversation is kept: this, and its id.
const ITEMS: &str = "session:";
/// The next chat tab's number (in global storage, so keys stay unique).
const NEXT_CHAT: &str = "next-chat";
pub const MAX_SESSIONS: usize = 16;
/// Most of a session's conversation kept, in bytes of JSON.
pub const ITEMS_BUDGET: usize = 40 * 1024;

fn get(scope: StorageScope, key: &str) -> Option<Value> {
  storage::get(scope, key).and_then(|text| serde_json::from_str(&text).ok())
}

fn set(scope: StorageScope, key: &str, value: Option<&Value>) {
  let text = value.map(Value::to_string);
  // A full storage loses history, nothing more.
  let _ = storage::set(scope, key, text.as_deref());
}

/// The sessions kept, most recently used first.
pub fn sessions() -> Vec<Value> {
  get(StorageScope::Workspace, INDEX)
    .and_then(|index| index.as_array().cloned())
    .unwrap_or_default()
}

/// Records a session's title, when it was used, and its conversation.
pub fn save(session: &str, title: &str, updated: f64, items: Vec<Value>) {
  let mut index = sessions();
  index.retain(|entry| entry["id"] != session);
  index.insert(
    0,
    json!({ "id": session, "title": title, "updated": updated }),
  );
  for forgotten in index.split_off(index.len().min(MAX_SESSIONS)) {
    if let Some(id) = forgotten["id"].as_str() {
      set(StorageScope::Workspace, &format!("{ITEMS}{id}"), None);
    }
  }
  set(StorageScope::Workspace, INDEX, Some(&Value::Array(index)));
  set(
    StorageScope::Workspace,
    &format!("{ITEMS}{session}"),
    Some(&Value::Array(items)),
  );
}

/// A session's title, if it's kept.
pub fn title(session: &str) -> Option<String> {
  sessions()
    .into_iter()
    .find(|entry| entry["id"] == session)
    .and_then(|entry| entry["title"].as_str().map(str::to_owned))
}

/// A session's conversation, as kept.
pub fn items(session: &str) -> Vec<Value> {
  get(StorageScope::Workspace, &format!("{ITEMS}{session}"))
    .and_then(|items| items.as_array().cloned())
    .unwrap_or_default()
}

fn tabs() -> Map<String, Value> {
  get(StorageScope::Workspace, TABS)
    .and_then(|tabs| tabs.as_object().cloned())
    .unwrap_or_default()
}

/// Remembers the session a chat tab holds, or forgets the tab (`None`).
pub fn set_tab(key: &str, session: Option<&str>) {
  let mut tabs = tabs();
  match session {
    Some(session) => tabs.insert(key.to_owned(), json!(session)),
    None => tabs.remove(key),
  };
  set(StorageScope::Workspace, TABS, Some(&Value::Object(tabs)));
}

/// The session a restored chat tab held.
pub fn tab(key: &str) -> Option<String> {
  tabs().get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A key for a new chat tab, not one of `open`.
pub fn next_chat(open: &[String]) -> String {
  let mut next = get(StorageScope::Global, NEXT_CHAT)
    .and_then(|next| next.as_u64())
    .unwrap_or(1);
  while open.contains(&next.to_string()) {
    next += 1;
  }
  set(StorageScope::Global, NEXT_CHAT, Some(&json!(next + 1)));
  next.to_string()
}
