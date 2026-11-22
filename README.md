# Claude Code for Flareite

Chat with [Claude Code](https://code.claude.com), Anthropic's coding agent, in a tab beside the
editor. Claude reads your project, edits files, and runs commands, asking you first, and you
review each edit as a diff before it's made.

This extension runs the `claude` program you installed, the same one you use in a terminal, so
it uses your Claude Code sign-in, settings, `CLAUDE.md` files, MCP servers, and skills.

## Getting started

1. [Install Claude Code](https://code.claude.com/docs/en/setup), and sign in once: run `claude`
   in a terminal, or **Claude Code: Sign In** from the command palette.
2. Open a folder, and trust it when asked (Claude Code runs programs in it).
3. Press **Ctrl+Esc** (**Cmd+Esc** on macOS), or click **✻ Claude** in the status bar.

## In the chat

- **Send** with Enter (Shift+Enter for a new line). While Claude works, **Stop** (or Escape)
  interrupts it; a message sent meanwhile waits its turn.
- **⧉ main.rs:12-18** above the message box sends the file you're in, and your selection, along
  with the message. Click it to leave them out.
- **Problems** writes out the problems in the file you're in (from its language server and
  extensions) for Claude to fix. **Fix Problems in This File** in the editor's menu does the same.
- **@** mentions a file of the folder; **/** runs one of Claude Code's commands, such as
  `/compact` or `/review`.
- **The model and mode menus** change the chat's model, and how Claude asks: **Ask before
  edits**, **Edit automatically**, **Plan mode** (Claude plans without editing, then asks you to
  approve the plan), **Auto mode** (Claude Code decides which actions are safe to take without
  asking; for models that have it), or **Bypass permissions** (Claude never asks; see below).
- **The effort menu** sets how hard Claude thinks, from **Low** to **Max**, for the rest of the
  chat; it lists the levels the chosen model takes. More effort is slower and uses more of your
  limits. **Default effort** names the level in use: the model's own, or the one in your Claude
  Code settings.
- **The menus remember** what you chose: a new chat starts with the last mode, model, and effort
  picked (they're saved as this extension's settings, below).
- **Past chats** (the list button) continue an earlier conversation in this folder. **Claude
  Code: Resume a Past Chat…** opens one in a new tab. Chat tabs come back with the window and
  carry on where they left off.

## Permissions

Before Claude edits a file, the chat shows the edit as a diff: **Accept**, **Accept All Edits**
(for the rest of the chat), or **Reject**, telling Claude what to do instead. **Open Diff** shows
the file as it is beside the file as Claude would leave it, with the same buttons. Commands show
what they'd run, with **Allow**, **Allow for This Chat**, or **Deny**. Claude's questions show
their choices, and its plans ask for approval.

To let Claude work without asking, choose **Bypass permissions** in the mode menu. The first time,
Flareite asks you to allow it (that sets `allow_bypass_permissions`); a chat that's already
running starts again to use it, continuing its conversation (after the current turn, if Claude is
working). Claude then runs commands and changes files without asking. Its commands aren't
limited to the folder: they can do anything you can on this computer. Its questions still wait for
your answer. **Auto mode** is the middle ground: Claude Code approves what it judges safe and
stops what it judges risky.

## Commands

| Command                                   | Keys            | What it does                                            |
| ----------------------------------------- | --------------- | ------------------------------------------------------- |
| Claude Code: Open Claude Code             | Ctrl+Esc        | Shows the chat used last, or a new one                  |
| Claude Code: New Chat                     | Ctrl+Shift+Esc  | Opens a new chat                                        |
| Claude Code: Add Selection to Chat        | Ctrl+Alt+K      | Mentions the selected lines (`@src/main.rs#L12-18`)     |
| Claude Code: Add to Chat                  | explorer's menu | Mentions a file or folder                               |
| Claude Code: Fix Problems in This File    | editor's menu   | Asks Claude to fix the file's problems                  |
| Claude Code: Resume a Past Chat…          |                 | Continues an earlier chat in a new tab                  |
| Claude Code: Run Claude in a Terminal     |                 | Runs `claude` in the integrated terminal                |
| Claude Code: Sign In                      |                 | Runs `claude auth login` in a terminal                  |
| Claude Code: Use an Anthropic API Key…    |                 | Uses an API key (kept in the system's credential store) |
| Claude Code: Forget the Anthropic API Key |                 | Goes back to Claude Code's own sign-in                  |

## Settings

```toml
[extensions.settings."flareite.claude-code"]
model = ""                      # the model new chats start with ("sonnet", "opus"); empty for the default
permission_mode = "default"     # default, acceptEdits, plan, auto, or bypassPermissions
allow_bypass_permissions = false  # whether Bypass permissions can be used
effort = ""                     # the effort new chats start with: low, medium, high, xhigh, or max; empty for the default
include_editor_context = true   # send the file in use and its selection by default
open_beside = true              # open chats beside the editor
```

If `claude` isn't on Flareite's `PATH`, say where it is:

```toml
[extensions.programs]
"flareite.claude-code.claude" = "C:/Users/me/.local/bin/claude.exe"
```

## How it works

The extension starts `claude --print --input-format stream-json --output-format stream-json` for
each chat, writes your messages to it, and reads its replies as they stream, with Flareite's
interactive programs (extension API 2.1). Claude Code's permission prompts come to the extension
(`--permission-prompt-tool stdio`), which shows them in the chat. It needs the **run programs**
permission for `claude`, **read the folder's files** to show edits against the files as they are
and to list files for @-mentions, and **read documents** to send your selection and the
problems in a file.

Claude Code is a product of Anthropic. This extension is part of Flareite, not made by Anthropic.

## Building

```sh
cargo build --release      # target/wasm32-unknown-unknown/release/claude_code.wasm
cargo test --target x86_64-pc-windows-msvc   # or your machine's target
```

It builds against the extension API in the editor's repository, checked out beside this one as
`editor`. Add this folder as a development extension (**Extensions: Add Development Extension…**)
and Flareite builds it as you edit.
