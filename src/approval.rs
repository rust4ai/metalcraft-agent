use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode};
use metalcraft::BeforeToolCallAction;
use crate::{diff_preview, ui};
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// How sensitive/destructive an operation is
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionLevel {
    /// Always auto-approved, no prompt needed
    AutoApprove,
    /// Prompt user for approval
    RequiresApproval,
}

/// Classification of what a tool call is actually doing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationKind {
    ReadFile,
    ListFiles,
    Search,        // grep, find_files
    WriteNewFile,  // write_file when path doesn't exist
    OverwriteFile, // write_file when path already exists
    EditFile,      // edit_file
    Execute,       // bash
    NetworkFetch,  // web_fetch
    SubAgent,      // sub_agent
    LoadSkill,     // load_skill
    DiscordAction, // discord_send_message, discord_edit_message, discord_add_reaction
    MetaRead,      // read-only meta tools (list/read/validate over the project's own files)
    MetaWrite,     // mutating meta tools (write/delete personas, skills, flows)
}

impl OperationKind {
    /// Classify a tool call into an OperationKind.
    pub fn classify(tool_name: &str, args: &serde_json::Value) -> Self {
        match tool_name {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "grep" | "find_files" => Self::Search,
            "write_file" => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !path.is_empty() && Path::new(path).exists() {
                    Self::OverwriteFile
                } else {
                    Self::WriteNewFile
                }
            }
            "edit_file" => Self::EditFile,
            "bash" => Self::Execute,
            "web_fetch" => Self::NetworkFetch,
            "sub_agent" => Self::SubAgent,
            "load_skill" => Self::LoadSkill,
            "discord_send_message" | "discord_edit_message" | "discord_add_reaction" => Self::DiscordAction,
            "discord_get_messages" | "discord_get_channel_info" => Self::ReadFile,
            // Meta tools — managing the project's own personas/skills/flows.
            // Read-only ones auto-approve; mutating ones require approval.
            "persona_list" | "persona_read" | "skill_list" | "skill_read" | "flow_list"
            | "flow_read" | "flow_validate" | "flow_templates_list" | "flow_template_read"
            | "diagnostics_list" | "diagnostics_read" => Self::MetaRead,
            "persona_write" | "persona_delete" | "skill_write" | "skill_delete" | "flow_write"
            | "flow_delete" => Self::MetaWrite,
            // flow_run spawns agent runs that may use any tool — treat it like
            // a sub-agent (requires approval).
            "flow_run" => Self::SubAgent,
            // Default unknown tools to Execute (requires approval)
            _ => Self::Execute,
        }
    }

    /// Default permission policy for each operation kind.
    pub fn default_permission(&self) -> PermissionLevel {
        match self {
            Self::ReadFile | Self::ListFiles | Self::Search | Self::LoadSkill => PermissionLevel::AutoApprove,
            Self::MetaRead => PermissionLevel::AutoApprove,
            Self::MetaWrite => PermissionLevel::RequiresApproval,
            Self::WriteNewFile => PermissionLevel::AutoApprove,
            Self::OverwriteFile => PermissionLevel::RequiresApproval,
            Self::EditFile => PermissionLevel::RequiresApproval,
            Self::Execute => PermissionLevel::RequiresApproval,
            Self::NetworkFetch => PermissionLevel::RequiresApproval,
            Self::SubAgent => PermissionLevel::RequiresApproval,
            Self::DiscordAction => PermissionLevel::RequiresApproval,
        }
    }

    /// Human-readable label for the prompt display.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ReadFile => "ReadFile",
            Self::ListFiles => "ListFiles",
            Self::Search => "Search",
            Self::WriteNewFile => "WriteNewFile",
            Self::OverwriteFile => "OverwriteFile",
            Self::EditFile => "EditFile",
            Self::Execute => "Execute",
            Self::NetworkFetch => "NetworkFetch",
            Self::SubAgent => "SubAgent",
            Self::LoadSkill => "LoadSkill",
            Self::DiscordAction => "DiscordAction",
            Self::MetaRead => "MetaRead",
            Self::MetaWrite => "MetaWrite",
        }
    }
}

#[derive(Clone)]
pub enum ApprovalMode {
    /// Always auto-approve everything.
    AutoApprove,
    /// Use data-driven permission policy with optional overrides.
    Interactive {
        /// Override default permissions for specific operation kinds.
        overrides: HashMap<OperationKind, PermissionLevel>,
    },
}

impl ApprovalMode {
    /// Default policy: uses OperationKind::default_permission() with no overrides.
    pub fn default_interactive() -> Self {
        Self::Interactive {
            overrides: HashMap::new(),
        }
    }
}

/// Build a `BeforeToolCallHook` for metalcraft based on approval mode.
pub fn build_hook(mode: ApprovalMode) -> Option<metalcraft::BeforeToolCallHook> {
    match mode {
        ApprovalMode::AutoApprove => None,
        ApprovalMode::Interactive { overrides } => {
            // Files the user has already approved modifying this session. Once a
            // path is approved, further overwrites/edits to it skip the prompt.
            let approved_paths: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
            Some(Arc::new(move |name: &str, args: &serde_json::Value| {
                let op = OperationKind::classify(name, args);
                let level = overrides
                    .get(&op)
                    .copied()
                    .unwrap_or_else(|| op.default_permission());
                match level {
                    PermissionLevel::AutoApprove => BeforeToolCallAction::Proceed,
                    PermissionLevel::RequiresApproval => {
                        let path = rememberable_path(&op, args);

                        // Already approved this file this session — don't re-prompt.
                        if let Some(p) = &path {
                            if approved_paths.lock().unwrap().contains(p) {
                                eprintln!(
                                    "  {}",
                                    ui::dim(format!("↳ auto-approved {p} (remembered this session)"))
                                );
                                return BeforeToolCallAction::Proceed;
                            }
                        }

                        let action = prompt_user(&op, name, args);

                        // Remember a file approval so we stop asking for it.
                        if matches!(action, BeforeToolCallAction::Proceed) {
                            if let Some(p) = path {
                                approved_paths.lock().unwrap().insert(p);
                            }
                        }
                        action
                    }
                }
            }))
        }
    }
}

/// The file path a per-file modification targets, if approving it should be
/// remembered for the rest of the session. Only file-modifying operations
/// qualify — execution, network, sub-agent and Discord actions are re-prompted
/// every time since their effects differ across calls.
fn rememberable_path(op: &OperationKind, args: &serde_json::Value) -> Option<String> {
    match op {
        OperationKind::OverwriteFile | OperationKind::EditFile => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn prompt_user(
    op: &OperationKind,
    tool_name: &str,
    args: &serde_json::Value,
) -> BeforeToolCallAction {
    let args_display = match tool_name {
        "bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("(no command)")
            .to_string(),
        "write_file" | "edit_file" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        _ => serde_json::to_string(args).unwrap_or_default(),
    };

    // Build header lines (always shown)
    let header = format!(
        "  {} {} {}",
        ui::warning("⚡"),
        ui::label(op.label()),
        ui::dim(format!("→ {}", args_display))
    );

    // Collect diff lines if applicable
    let diff_lines = collect_diff_lines(tool_name, args);

    let result = if diff_lines.is_empty() {
        // No diff — use simple inline approval (bash commands, etc.)
        eprintln!();
        eprintln!("{header}");
        eprintln!();
        simple_approval_prompt()
    } else {
        // Has diff — check if it fits on screen or needs scrollable viewer
        let term_height = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let footer_lines = 4; // blank + "Approve?" + options + hints
        let header_lines = 3; // blank + header + blank
        let viewport_height = term_height.saturating_sub(footer_lines + header_lines);

        if diff_lines.len() <= viewport_height {
            // Small diff — print inline and use simple approval
            eprintln!();
            eprintln!("{header}");
            eprintln!();
            for line in &diff_lines {
                eprintln!("{line}");
            }
            eprintln!();
            simple_approval_prompt()
        } else {
            // Large diff — use interactive scrollable viewer
            interactive_diff_approval(&header, &diff_lines)
        }
    };

    // Echo the decision so the user gets immediate confirmation that their
    // keypress registered. Raw mode is already torn down by this point.
    match &result {
        Ok(true) => eprintln!("  {}", ui::success("✓ Approved")),
        Ok(false) => eprintln!("  {}", ui::error("✗ Denied")),
        Err(_) => {}
    }

    match result {
        Ok(true) => BeforeToolCallAction::Proceed,
        Ok(false) => BeforeToolCallAction::Deny(format!("User denied tool '{tool_name}'")),
        Err(err) => BeforeToolCallAction::Deny(format!("Failed to read approval input: {err}")),
    }
}

/// Collect diff preview lines for edit_file / write_file operations.
fn collect_diff_lines(tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    match tool_name {
        "edit_file" => {
            if let (Some(path), Some(old_s), Some(new_s)) = (
                args.get("path").and_then(|v| v.as_str()),
                args.get("old_string").and_then(|v| v.as_str()),
                args.get("new_string").and_then(|v| v.as_str()),
            ) {
                let diff = diff_preview::preview_file_edit(path, old_s, new_s);
                diff.lines().map(|l| l.to_string()).collect()
            } else {
                Vec::new()
            }
        }
        "write_file" => {
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                if let (Ok(old_content), Some(new_content)) = (
                    std::fs::read_to_string(path),
                    args.get("content").and_then(|v| v.as_str()),
                ) {
                    let diff = diff_preview::preview_file_edit(path, &old_content, new_content);
                    diff.lines().map(|l| l.to_string()).collect()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Discard any keystrokes buffered before the prompt appeared (type-ahead).
///
/// While the agent is working, rustyline is not reading, so anything the user
/// types sits in the terminal's input buffer. Without draining it, that stale
/// input gets consumed by the approval prompt's `event::read()` and can
/// silently answer (often deny) the prompt. Must be called in raw mode.
fn drain_pending_input() {
    while event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }
}

/// Simple inline approval prompt (no scrolling needed).
fn simple_approval_prompt() -> io::Result<bool> {
    eprintln!("  {}", ui::command("Approve?"));
    eprintln!("    1. Yes");
    eprintln!("    2. No");
    eprintln!("  {}", ui::dim("Use ↑/↓, Enter, or press y/n."));

    run_on_thread(|| simple_approval_inner())
}

fn simple_approval_inner() -> io::Result<bool> {
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }

    let mut selected = 0usize;
    enable_raw_mode()?;
    let _guard = RawModeGuard;
    drain_pending_input();

    loop {
        render_approval_menu(selected)?;

        // Block until the user presses a key. No timeout: we wait indefinitely
        // rather than auto-denying, since the agent has nothing to do until the
        // user responds anyway.
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => return Ok(selected == 0),
                KeyCode::Up => selected = 0,
                KeyCode::Down => selected = 1,
                KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
                KeyCode::Char('2') | KeyCode::Char('n') | KeyCode::Char('N') => return Ok(false),
                _ => {}
            }
        }
    }
}

/// Interactive scrollable diff viewer with integrated approval prompt.
fn interactive_diff_approval(header: &str, diff_lines: &[String]) -> io::Result<bool> {
    let header = header.to_string();
    let diff_lines = diff_lines.to_vec();
    run_on_thread(move || interactive_diff_inner(&header, &diff_lines))
}

fn interactive_diff_inner(header: &str, diff_lines: &[String]) -> io::Result<bool> {
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            // Show cursor, leave alternate screen
            let _ = crossterm::execute!(
                io::stderr(),
                crossterm::cursor::Show,
                terminal::LeaveAlternateScreen
            );
        }
    }

    // Enter alternate screen for clean rendering
    crossterm::execute!(
        io::stderr(),
        terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    enable_raw_mode()?;
    let _guard = RawModeGuard;
    drain_pending_input();

    let mut scroll_offset: usize = 0;
    let mut selected: usize = 0; // 0 = Yes, 1 = No
    let total_lines = diff_lines.len();

    loop {
        let (term_w, term_h) = terminal::size().unwrap_or((80, 24));
        let term_h = term_h as usize;
        let term_w = term_w as usize;

        // Layout: header (2 lines) + viewport + footer (4 lines)
        let header_rows = 2;
        let footer_rows = 4;
        let viewport_h = term_h.saturating_sub(header_rows + footer_rows);
        let max_scroll = total_lines.saturating_sub(viewport_h);
        scroll_offset = cmp::min(scroll_offset, max_scroll);

        let mut out = io::stderr();

        // Move to top-left, clear screen
        write!(out, "\x1b[H\x1b[2J")?;

        // Header
        writeln!(out, "{header}\r")?;

        // Scroll indicator
        let scroll_info = if total_lines > viewport_h {
            let end_line = cmp::min(scroll_offset + viewport_h, total_lines);
            format!("[{}-{}/{}]", scroll_offset + 1, end_line, total_lines)
        } else {
            format!("[{} lines]", total_lines)
        };
        writeln!(out, "  {}\r", ui::dim(&scroll_info))?;

        // Viewport: show diff lines [scroll_offset .. scroll_offset + viewport_h]
        let visible_end = cmp::min(scroll_offset + viewport_h, total_lines);
        for i in scroll_offset..visible_end {
            // Truncate long lines to terminal width to avoid wrapping
            let line = &diff_lines[i];
            let display = truncate_to_width(line, term_w);
            writeln!(out, "{display}\r")?;
        }

        // Fill remaining viewport lines with empty
        for _ in (visible_end - scroll_offset)..viewport_h {
            writeln!(out, "\r")?;
        }

        // Footer: blank, Approve?, options, hints
        writeln!(out, "\r")?;
        writeln!(out, "  {}\r", ui::command("Approve?"))?;
        if selected == 0 {
            writeln!(out, "  {}    2. No\r", ui::success("> 1. Yes"))?;
        } else {
            writeln!(out, "    1. Yes    {}\r", ui::error("> 2. No"))?;
        }
        write!(
            out,
            "  {}",
            ui::dim("PgUp/PgDn: scroll │ ↑/↓: select │ y/n/Enter: confirm")
        )?;

        out.flush()?;

        // Block until the user presses a key. No timeout: we wait indefinitely
        // rather than auto-denying.
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                // Scrolling
                KeyCode::PageUp => {
                    scroll_offset = scroll_offset.saturating_sub(viewport_h);
                }
                KeyCode::PageDown => {
                    scroll_offset = cmp::min(scroll_offset + viewport_h, max_scroll);
                }
                KeyCode::Home => {
                    scroll_offset = 0;
                }
                KeyCode::End => {
                    scroll_offset = max_scroll;
                }
                // Approval selection
                KeyCode::Up => selected = 0,
                KeyCode::Down => selected = 1,
                KeyCode::Enter => return Ok(selected == 0),
                KeyCode::Char('1') | KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
                KeyCode::Char('2') | KeyCode::Char('n') | KeyCode::Char('N') => return Ok(false),
                KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(false),
                KeyCode::Esc => return Ok(false),
                _ => {}
            }
        }
    }
}

/// Truncate a string (stripping ANSI) to fit terminal width.
/// Simple approach: just cut at byte boundary if too long.
fn truncate_to_width(line: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    // Count visible characters (skip ANSI escape sequences)
    let mut visible = 0;
    let mut last_safe_byte = 0;
    let mut in_escape = false;
    for (i, ch) in line.char_indices() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            last_safe_byte = i + ch.len_utf8();
            continue;
        }
        if ch == '\x1b' {
            in_escape = true;
            last_safe_byte = i + ch.len_utf8();
            continue;
        }
        visible += 1;
        last_safe_byte = i + ch.len_utf8();
        if visible >= max_width {
            break;
        }
    }
    &line[..last_safe_byte]
}

fn render_approval_menu(selected: usize) -> io::Result<()> {
    eprint!("\r\x1b[2K  ");
    if selected == 0 {
        eprint!("{}    2. No", ui::success("> 1. Yes"));
    } else {
        eprint!("  1. Yes    {}", ui::error("> 2. No"));
    }
    io::stderr().flush()
}

/// Run a closure on a dedicated OS thread (avoids blocking tokio runtime
/// and isolates terminal state from rustyline).
fn run_on_thread<F>(f: F) -> io::Result<bool>
where
    F: FnOnce() -> io::Result<bool> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_read_operations() {
        let args = serde_json::json!({});
        assert_eq!(OperationKind::classify("read_file", &args), OperationKind::ReadFile);
        assert_eq!(OperationKind::classify("list_files", &args), OperationKind::ListFiles);
        assert_eq!(OperationKind::classify("grep", &args), OperationKind::Search);
        assert_eq!(OperationKind::classify("find_files", &args), OperationKind::Search);
    }

    #[test]
    fn test_classify_write_new_file() {
        let args = serde_json::json!({"path": "/tmp/nonexistent_metalcraft_test_file_xyz.txt"});
        assert_eq!(OperationKind::classify("write_file", &args), OperationKind::WriteNewFile);
    }

    #[test]
    fn test_classify_other_tools() {
        let args = serde_json::json!({});
        assert_eq!(OperationKind::classify("edit_file", &args), OperationKind::EditFile);
        assert_eq!(OperationKind::classify("bash", &args), OperationKind::Execute);
        assert_eq!(OperationKind::classify("web_fetch", &args), OperationKind::NetworkFetch);
        assert_eq!(OperationKind::classify("sub_agent", &args), OperationKind::SubAgent);
        assert_eq!(OperationKind::classify("load_skill", &args), OperationKind::LoadSkill);
    }

    #[test]
    fn test_classify_meta_tools() {
        let args = serde_json::json!({});
        // Read-only meta tools auto-approve.
        for t in ["persona_list", "persona_read", "skill_read", "flow_list", "flow_validate", "diagnostics_read"] {
            assert_eq!(OperationKind::classify(t, &args), OperationKind::MetaRead, "{t}");
            assert_eq!(OperationKind::MetaRead.default_permission(), PermissionLevel::AutoApprove);
        }
        // Mutating meta tools require approval.
        for t in ["persona_write", "persona_delete", "skill_write", "flow_write", "flow_delete"] {
            assert_eq!(OperationKind::classify(t, &args), OperationKind::MetaWrite, "{t}");
            assert_eq!(OperationKind::MetaWrite.default_permission(), PermissionLevel::RequiresApproval);
        }
        // flow_run spawns agent work — gated like a sub-agent.
        assert_eq!(OperationKind::classify("flow_run", &args), OperationKind::SubAgent);
    }

    #[test]
    fn test_default_permissions() {
        assert_eq!(OperationKind::ReadFile.default_permission(), PermissionLevel::AutoApprove);
        assert_eq!(OperationKind::ListFiles.default_permission(), PermissionLevel::AutoApprove);
        assert_eq!(OperationKind::Search.default_permission(), PermissionLevel::AutoApprove);
        assert_eq!(OperationKind::WriteNewFile.default_permission(), PermissionLevel::AutoApprove);
        assert_eq!(OperationKind::OverwriteFile.default_permission(), PermissionLevel::RequiresApproval);
        assert_eq!(OperationKind::EditFile.default_permission(), PermissionLevel::RequiresApproval);
        assert_eq!(OperationKind::Execute.default_permission(), PermissionLevel::RequiresApproval);
    }

    #[test]
    fn test_unknown_tool_defaults_to_execute() {
        let args = serde_json::json!({});
        assert_eq!(OperationKind::classify("unknown_tool", &args), OperationKind::Execute);
        assert_eq!(OperationKind::Execute.default_permission(), PermissionLevel::RequiresApproval);
    }
}
