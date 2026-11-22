//! Claude's Markdown as HTML for the chat page.
//!
//! The page puts this HTML in its document, so nothing in it may run or
//! load: HTML in the Markdown is shown as text, links keep only web, mail,
//! and relative addresses (the page opens them through the extension), and
//! pictures become their description.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

/// Markdown as HTML that's safe to show.
pub fn to_html(markdown: &str) -> String {
  let options = Options::ENABLE_TABLES
    | Options::ENABLE_STRIKETHROUGH
    | Options::ENABLE_TASKLISTS
    | Options::ENABLE_GFM;
  let events = Parser::new_ext(markdown, options).map(|event| match event {
    Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
    Event::Start(Tag::Link {
      link_type,
      dest_url,
      title,
      id,
    }) => Event::Start(Tag::Link {
      link_type,
      dest_url: safe_address(dest_url),
      title,
      id,
    }),
    // The description stays, as text in brackets.
    Event::Start(Tag::Image { .. }) => Event::Text("[".into()),
    Event::End(TagEnd::Image) => Event::Text("]".into()),
    other => other,
  });
  let mut out = String::new();
  html::push_html(&mut out, events);
  out
}

/// `address` if it's a web or mail address, a heading, or a relative path
/// (a file Claude names); otherwise nothing.
fn safe_address(address: CowStr<'_>) -> CowStr<'_> {
  let lower = address.trim().to_ascii_lowercase();
  let scheme = lower
    .find(':')
    .filter(|&at| !lower[..at].contains(['/', '\\', '#', '?']))
    .map(|at| &lower[..at]);
  match scheme {
    None => address,
    Some("http" | "https" | "mailto") => address,
    // A Windows path, `C:\src\main.rs`.
    Some(drive) if drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic()) => address,
    Some(_) => "".into(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn markdown_is_rendered() {
    let html = to_html("# Title\n\n**bold** and `code`\n\n```rust\nfn x() {}\n```\n");
    assert!(html.contains("<h1>Title</h1>"), "{html}");
    assert!(html.contains("<strong>bold</strong>"));
    assert!(html.contains("<code>code</code>"));
    assert!(html.contains("<code class=\"language-rust\">fn x() {}"));
  }

  #[test]
  fn html_is_shown_as_text() {
    let html = to_html("hi <script>alert(1)</script>\n\n<div onclick=\"x\">block</div>\n");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("<div"), "{html}");
    assert!(html.contains("&lt;script&gt;"));
  }

  #[test]
  fn only_safe_link_addresses_stay() {
    let html = to_html(
      "[a](https://example.com) [b](javascript:alert(1)) [c](src/main.rs#L3) [d](C:\\x.rs)",
    );
    assert!(html.contains("href=\"https://example.com\""), "{html}");
    assert!(!html.contains("javascript"), "{html}");
    assert!(html.contains("href=\"src/main.rs#L3\""));
    assert!(
      html.contains("href=\"C:%5Cx.rs\"") || html.contains("href=\"C:\\x.rs\""),
      "{html}"
    );
  }

  #[test]
  fn pictures_become_their_description() {
    let html = to_html("![a cat](https://example.com/cat.png)");
    assert!(!html.contains("<img"), "{html}");
    assert!(html.contains("[a cat]"));
  }
}
