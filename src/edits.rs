//! Edits Claude proposes: the file as it is, and as Claude's tool would
//! leave it.
//!
//! When Claude asks to use `Write`, `Edit`, or `MultiEdit`, the extension
//! reads the file as it is (from the editor when it's open there, with
//! unsaved changes) and works out the text the tool would write. The chat
//! shows the difference as hunks in the permission card; **Open Diff**
//! shows it in an `edit` layout tab, the two texts side by side as
//! `claude-code:` addresses this extension provides, with Accept and Reject
//! buttons.

use std::ops::Range;

use flareite_diff::{DiffOptions, LineChange};
use flareite_extension_api::layouts::{
  self, Button, Decorated, Direction, EditorPane, Layout, LayoutNode, LayoutTab, LineRange,
  NodeKind, Pane, PaneContent, PaneDecorations, RulerMark, Zone, ZoneContent,
};
use flareite_extension_api::types::{Position, Range as ApiRange};
use serde_json::{Value, json};

use crate::protocol::text;

pub const LAYOUT: &str = "edit";
pub const SCHEME: &str = "claude-code";
/// Unchanged lines shown around each change in the chat.
const CONTEXT: usize = 3;
/// Most lines of hunks sent to the chat; the diff tab shows the rest.
const MAX_HUNK_LINES: usize = 2_000;
/// Longest line sent to the chat, in characters.
const MAX_LINE_CHARS: usize = 500;

/// An edit Claude proposes.
#[derive(Debug, Clone)]
pub struct ProposedEdit {
  /// The permission request it's for.
  pub request: String,
  /// The file, as Claude names it (absolute).
  pub path: String,
  pub original: String,
  pub proposed: String,
  /// The text to replace wasn't found in the file: the texts are the
  /// tool's old and new strings only.
  pub approximate: bool,
  pub changes: Vec<LineChange>,
}

/// The file a tool would change, if it's one that edits files.
pub fn edited_file<'a>(tool: &str, input: &'a Value) -> Option<&'a str> {
  match tool {
    "Write" | "Edit" | "MultiEdit" => text(input, "file_path"),
    _ => None,
  }
}

/// One replacement of an `Edit` or `MultiEdit`.
struct Replacement<'a> {
  old: &'a str,
  new: &'a str,
  all: bool,
}

fn replacement(value: &Value) -> Option<Replacement<'_>> {
  Some(Replacement {
    old: text(value, "old_string")?,
    new: text(value, "new_string").unwrap_or_default(),
    all: value
      .get("replace_all")
      .and_then(Value::as_bool)
      .unwrap_or(false),
  })
}

/// Makes `replacement` in `text`; `None` if its old text isn't there. A
/// file with Windows line breaks is matched with the replacement's `\n`s
/// made `\r\n`.
fn replace(text: &str, replacement: &Replacement<'_>) -> Option<String> {
  let attempt = |old: &str, new: &str| -> Option<String> {
    if old.is_empty() || !text.contains(old) {
      return None;
    }
    Some(if replacement.all {
      text.replace(old, new)
    } else {
      text.replacen(old, new, 1)
    })
  };
  attempt(replacement.old, replacement.new).or_else(|| {
    text.contains("\r\n").then_some(())?;
    let crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
    attempt(&crlf(replacement.old), &crlf(replacement.new))
  })
}

/// The texts of a proposed edit: the file's `current` text (`None` when it
/// doesn't exist or can't be read) and the text the tool would leave, and
/// whether they're only the tool's strings. `None` for other tools.
pub fn texts(tool: &str, input: &Value, current: Option<&str>) -> Option<(String, String, bool)> {
  let current_text = current.unwrap_or_default().to_owned();
  match tool {
    "Write" => Some((
      current_text,
      text(input, "content").unwrap_or_default().to_owned(),
      false,
    )),
    "Edit" | "MultiEdit" => {
      let replacements: Vec<Replacement<'_>> = if tool == "Edit" {
        replacement(input).into_iter().collect()
      } else {
        input
          .get("edits")
          .and_then(Value::as_array)
          .map(|edits| edits.iter().filter_map(replacement).collect())
          .unwrap_or_default()
      };
      let mut proposed = current_text.clone();
      for one in &replacements {
        match current.and_then(|_| replace(&proposed, one)) {
          Some(next) => proposed = next,
          None => {
            // Only what's replaced, and what replaces it.
            let old: Vec<&str> = replacements.iter().map(|one| one.old).collect();
            let new: Vec<&str> = replacements.iter().map(|one| one.new).collect();
            return Some((old.join("\n…\n"), new.join("\n…\n"), true));
          }
        }
      }
      Some((current_text, proposed, false))
    }
    _ => None,
  }
}

/// The changes between an edit's two texts.
pub fn compare(original: &str, proposed: &str) -> Vec<LineChange> {
  flareite_diff::diff(original, proposed, DiffOptions::default()).unwrap_or_default()
}

/// A text's lines, without their line breaks (as `flareite-diff` splits
/// them).
fn lines(text: &str) -> Vec<&str> {
  text
    .split('\n')
    .map(|line| line.strip_suffix('\r').unwrap_or(line))
    .collect()
}

fn cut(line: &str) -> String {
  match line.char_indices().nth(MAX_LINE_CHARS) {
    Some((end, _)) => format!("{}…", &line[..end]),
    None => line.to_owned(),
  }
}

/// The changes as hunks for the chat: `{ added, removed, hunks, truncated }`,
/// each hunk's `lines` being `[kind, old number, new number, text]` with
/// kind ` `, `-`, or `+` and numbers from 1 (`null` on the other side).
pub fn hunks(original: &str, proposed: &str, changes: &[LineChange]) -> Value {
  let before = lines(original);
  let after = lines(proposed);
  // A new file's "line" is empty: it has none.
  let before: &[&str] = if original.is_empty() { &[] } else { &before };
  // The empty "line" after a final line break isn't shown as context.
  let shown_lines = before.len() - usize::from(original.ends_with('\n'));
  let before = &before[..shown_lines.min(before.len())];
  let mut hunks: Vec<Value> = Vec::new();
  let mut shown = 0;
  let mut truncated = false;
  let (mut added, mut removed) = (0, 0);
  let mut index = 0;
  while index < changes.len() {
    // Changes whose context touches join one hunk.
    let mut end = index + 1;
    while end < changes.len()
      && changes[end].original.start <= changes[end - 1].original.end + 2 * CONTEXT
    {
      end += 1;
    }
    let group = &changes[index..end];
    let first = &group[0];
    let start = first.original.start.saturating_sub(CONTEXT);
    // How far the new lines are from the old: unchanged lines keep it.
    let mut offset = first.modified.start as isize - first.original.start as isize;
    let mut rows: Vec<Value> = Vec::new();
    let mut old_line = start;
    let context = |rows: &mut Vec<Value>, range: Range<usize>, offset: isize| {
      for line in range {
        if let Some(text) = before.get(line) {
          let new = (line as isize + offset) as usize;
          rows.push(json!([" ", line + 1, new + 1, cut(text)]));
        }
      }
    };
    for change in group {
      context(&mut rows, old_line..change.original.start, offset);
      for line in change.original.clone() {
        if let Some(text) = before.get(line) {
          rows.push(json!(["-", line + 1, null, cut(text)]));
          removed += 1;
        }
      }
      for line in change.modified.clone() {
        if let Some(text) = after.get(line) {
          rows.push(json!(["+", null, line + 1, cut(text)]));
          added += 1;
        }
      }
      old_line = change.original.end;
      offset = change.modified.end as isize - change.original.end as isize;
    }
    let last = group.last().map_or(0, |change| change.original.end);
    context(
      &mut rows,
      old_line..(last + CONTEXT).min(before.len()),
      offset,
    );
    shown += rows.len();
    if shown > MAX_HUNK_LINES {
      truncated = true;
      break;
    }
    hunks.push(json!({ "lines": rows }));
    index = end;
  }
  json!({ "added": added, "removed": removed, "hunks": hunks, "truncated": truncated })
}

// --- The diff tab ------------------------------------------------------------

/// The name a path ends with.
pub fn file_name(path: &str) -> &str {
  path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The address of one side of an edit, which ends with the file's name so
/// the editor names the tab and picks the language by it.
pub fn address(edit: &str, side: &str, path: &str) -> String {
  format!("{SCHEME}:{edit}/{side}/{}", file_name(path))
}

/// The edit and side an address names.
pub fn parse_address(uri: &str) -> Option<(&str, &str)> {
  let rest = uri.strip_prefix(SCHEME)?.strip_prefix(':')?;
  let mut parts = rest.splitn(3, '/');
  Some((parts.next()?, parts.next()?))
}

pub fn tab(edit: &str) -> LayoutTab {
  LayoutTab {
    layout: LAYOUT.into(),
    key: edit.into(),
  }
}

/// A button of the diff tab; without an icon, its title is its text.
fn button(
  id: &str,
  title: &str,
  icon: Option<&str>,
  command: &str,
  edit: &str,
  primary: bool,
) -> Button {
  Button {
    id: id.into(),
    title: title.into(),
    icon: icon.map(str::to_owned),
    command: format!("flareite.claude-code.{command}"),
    argument: Some(edit.into()),
    primary,
    pressed: None,
    disabled: false,
  }
}

/// The diff tab's layout: the file as it is, and as Claude would leave it.
pub fn layout(id: &str, edit: &ProposedEdit) -> Layout {
  let editor = |side: &str| {
    NodeKind::Pane(Pane {
      content: PaneContent::Editor(EditorPane {
        uri: address(id, side, &edit.path),
        minimap: false,
        folding: false,
        quick_diff: false,
      }),
      header: None,
      scroll_group: Some("sides".into()),
    })
  };
  let node = |id: &str, parent: Option<&str>, kind| LayoutNode {
    id: id.into(),
    parent: parent.map(str::to_owned),
    size: 1.0,
    kind,
  };
  Layout {
    title: if edit.approximate {
      format!(
        "{} (Claude's Edit, Replaced Text Only)",
        file_name(&edit.path)
      )
    } else {
      format!("{} (Claude's Edit)", file_name(&edit.path))
    },
    nodes: vec![
      node("sides", None, NodeKind::Split(Direction::Row)),
      node("original", Some("sides"), editor("original")),
      node("proposed", Some("sides"), editor("proposed")),
    ],
    documents: Vec::new(),
    buttons: vec![
      button("accept", "Accept", Some("check"), "acceptEdit", id, true),
      button("acceptAll", "Accept All", None, "acceptAllEdits", id, false),
      button("reject", "Reject", Some("close"), "rejectEdit", id, false),
    ],
    breakpoints: Vec::new(),
    main: Some("proposed".into()),
  }
}

fn u32_of(value: usize) -> u32 {
  u32::try_from(value).unwrap_or(u32::MAX)
}

/// The UTF-8 column of UTF-16 column `column` in `line`.
fn utf8_column(line: &str, column: usize) -> u32 {
  let mut units = 0;
  for (index, c) in line.char_indices() {
    if units >= column {
      return u32_of(index);
    }
    units += c.len_utf16();
  }
  u32_of(line.len())
}

fn whole_lines(lines: &Range<usize>) -> ApiRange {
  let at = |line: usize| Position {
    line: u32_of(line),
    column: 0,
  };
  ApiRange {
    start: at(lines.start),
    end: at(lines.end),
  }
}

/// What the diff tab's panes show besides their text.
pub fn pane_decorations(edit: &ProposedEdit) -> [(&'static str, PaneDecorations); 2] {
  let before = lines(&edit.original);
  let after = lines(&edit.proposed);
  let changes = &edit.changes;
  let chars = |lines: &[&str], original: bool| -> Vec<ApiRange> {
    changes
      .iter()
      .flat_map(|change| &change.chars)
      .map(|chars| {
        let range = if original {
          &chars.original
        } else {
          &chars.modified
        };
        let at = |point: flareite_diff::Point| Position {
          line: u32_of(point.line),
          column: lines
            .get(point.line)
            .map_or(0, |text| utf8_column(text, point.column)),
        };
        ApiRange {
          start: at(range.start),
          end: at(range.end),
        }
      })
      .collect()
  };
  let changed = |original: bool| -> Vec<ApiRange> {
    changes
      .iter()
      .map(|change| {
        if original {
          &change.original
        } else {
          &change.modified
        }
      })
      .filter(|lines| !lines.is_empty())
      .map(whole_lines)
      .collect()
  };
  let decorated = |decoration: &str, ranges: Vec<ApiRange>| Decorated {
    decoration: decoration.into(),
    ranges,
  };
  let filler = |line: usize, rows: usize| Zone {
    line: u32_of(line),
    content: ZoneContent::Filler(u32_of(rows)),
    decoration: None,
  };
  let mut original = PaneDecorations {
    decorations: vec![
      decorated("removed-line", changed(true)),
      decorated("removed-text", chars(&before, true)),
    ],
    zones: Vec::new(),
    folds: Vec::new(),
    ruler: Vec::new(),
  };
  let mut proposed = PaneDecorations {
    decorations: vec![
      decorated("inserted-line", changed(false)),
      decorated("inserted-text", chars(&after, false)),
    ],
    zones: Vec::new(),
    folds: Vec::new(),
    ruler: changes
      .iter()
      .map(|change| RulerMark {
        lines: LineRange {
          start: u32_of(change.modified.start),
          end: u32_of(change.modified.end.max(change.modified.start + 1)),
        },
        decoration: match (change.original.is_empty(), change.modified.is_empty()) {
          (false, false) => "ruler-changed",
          (true, _) => "ruler-inserted",
          (false, true) => "ruler-removed",
        }
        .into(),
      })
      .collect(),
  };
  // Filler on the side with fewer lines keeps the lines after a change level.
  for change in changes {
    let (removed, inserted) = (change.original.len(), change.modified.len());
    if removed < inserted {
      original
        .zones
        .push(filler(change.original.end, inserted - removed));
    } else if inserted < removed {
      proposed
        .zones
        .push(filler(change.modified.end, removed - inserted));
    }
  }
  [("original", original), ("proposed", proposed)]
}

/// Opens an edit's diff tab, or shows it.
pub fn open(id: &str) -> Result<(), String> {
  layouts::open(&tab(id), true).map_err(|error| format!("{error:?}"))
}

/// Fills an edit's diff tab in: its layout and decorations.
pub fn fill(id: &str, edit: &ProposedEdit) {
  if layouts::set_layout(&tab(id), &layout(id, edit)).unwrap_or(false) {
    decorate(id, edit);
  }
}

/// Draws an edit's changes in its diff tab's panes.
pub fn decorate(id: &str, edit: &ProposedEdit) {
  for (pane, decorations) in pane_decorations(edit) {
    let _ = layouts::set_pane_decorations(&tab(id), pane, &decorations);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn writes_replace_the_whole_file() {
    let input = json!({ "file_path": "/w/a.txt", "content": "new\n" });
    assert_eq!(edited_file("Write", &input), Some("/w/a.txt"));
    assert_eq!(
      texts("Write", &input, Some("old\n")),
      Some(("old\n".into(), "new\n".into(), false))
    );
    assert_eq!(
      texts("Write", &input, None),
      Some((String::new(), "new\n".into(), false)),
      "a new file"
    );
  }

  #[test]
  fn edits_replace_once_or_everywhere() {
    let edit = json!({ "file_path": "a", "old_string": "x", "new_string": "y" });
    assert_eq!(
      texts("Edit", &edit, Some("x x")).map(|t| t.1),
      Some("y x".into())
    );
    let all =
      json!({ "file_path": "a", "old_string": "x", "new_string": "y", "replace_all": true });
    assert_eq!(
      texts("Edit", &all, Some("x x")).map(|t| t.1),
      Some("y y".into())
    );
    let multi = json!({ "file_path": "a", "edits": [
      { "old_string": "a", "new_string": "b" },
      { "old_string": "b", "new_string": "c" },
    ] });
    assert_eq!(
      texts("MultiEdit", &multi, Some("a")).map(|t| t.1),
      Some("c".into()),
      "each applies to the text left by the one before"
    );
  }

  #[test]
  fn windows_line_breaks_still_match() {
    let edit = json!({ "file_path": "a", "old_string": "one\ntwo", "new_string": "1\n2" });
    assert_eq!(
      texts("Edit", &edit, Some("zero\r\none\r\ntwo\r\n")).map(|t| t.1),
      Some("zero\r\n1\r\n2\r\n".into())
    );
  }

  #[test]
  fn missing_text_shows_only_the_strings() {
    let edit = json!({ "file_path": "a", "old_string": "gone", "new_string": "new" });
    assert_eq!(
      texts("Edit", &edit, Some("text")),
      Some(("gone".into(), "new".into(), true))
    );
    assert_eq!(texts("Bash", &edit, Some("text")), None);
  }

  #[test]
  fn hunks_show_changes_with_context() {
    let original = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n";
    let proposed = original
      .replacen("\n2\n", "\ntwo\n", 1)
      .replacen("18\n", "", 1);
    let changes = compare(original, &proposed);
    let shown = hunks(original, &proposed, &changes);
    assert_eq!(shown["added"], 1);
    assert_eq!(shown["removed"], 2);
    let hunks = shown["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 2, "far apart: two hunks");
    let first: Vec<(String, String)> = hunks[0]["lines"]
      .as_array()
      .expect("lines")
      .iter()
      .map(|row| {
        (
          row[0].as_str().unwrap().to_owned(),
          row[3].as_str().unwrap().to_owned(),
        )
      })
      .collect();
    assert_eq!(
      first,
      [
        (" ", "1"),
        ("-", "2"),
        ("+", "two"),
        (" ", "3"),
        (" ", "4"),
        (" ", "5")
      ]
      .map(|(kind, text)| (kind.to_owned(), text.to_owned()))
    );
    // Numbers on both sides, from 1.
    assert_eq!(hunks[0]["lines"][3], json!([" ", 3, 3, "3"]));
    assert_eq!(hunks[1]["lines"][3], json!(["-", 18, null, "18"]));
  }

  #[test]
  fn a_new_file_is_all_added() {
    let changes = compare("", "a\nb");
    let shown = hunks("", "a\nb", &changes);
    assert_eq!(shown["added"], 2);
    assert_eq!(shown["removed"], 0);
  }

  #[test]
  fn addresses_name_the_edit_side_and_file() {
    let uri = address("e3", "proposed", "C:\\w\\src\\main.rs");
    assert_eq!(uri, "claude-code:e3/proposed/main.rs");
    assert_eq!(parse_address(&uri), Some(("e3", "proposed")));
    assert_eq!(parse_address("git:HEAD:a"), None);
  }
}
