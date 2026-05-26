use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use metalcraft::BeforeToolCallAction;
use crate::{diff_preview, ui};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

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
            // Default unknown tools to Execute (requires approval)
            _ => Self::Execute,
        }
    }

    /// Default permission policy for each operation kind.
    pub fn default_permission(&self) -> PermissionLevel {
        match self {
            Self::ReadFile | Self::ListFiles | Self::Search | Self::LoadSkill => PermissionLevel::AutoApprove,
            Self::WriteNewFile => PermissionLevel::AutoApprove,
            Self::OverwriteFile => PermissionLevel::RequiresApproval,
            Self::EditFile => PermissionLevel::RequiresApproval,
            Self::Execute => PermissionLevel::RequiresApproval,
            Self::NetworkFetch => PermissionLevel::RequiresApproval,
            Self::SubAgent => PermissionLevel::RequiresApproval,
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
            Some(Arc::new(move |name: &str, args: &serde_json::Value| {
                let op = OperationKind::classify(name, args);
                let level = overrides
                    .get(&op)
                    .copied()
                    .unwrap_or_else(|| op.default_permission());
                match level {
                    PermissionLevel::AutoApprove => BeforeToolCallAction::Proceed,
                    PermissionLevel::RequiresApproval => prompt_user(&op, name, args),
                }
            }))
        }
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

    eprintln!();
    eprintln!(
        "  {} {} {}",
        ui::warning("⚡"),
        ui::label(op.label()),
        ui::dim(format!("→ {}", args_display))
    );
    eprintln!();

    // Show diff preview for edit_file (with file context + line numbers)
    if tool_name == "edit_file" {
        if let (Some(path), Some(old_s), Some(new_s)) = (
            args.get("path").and_then(|v| v.as_str()),
            args.get("old_string").and_then(|v| v.as_str()),
            args.get("new_string").and_then(|v| v.as_str()),
        ) {
            let diff = diff_preview::preview_file_edit(path, old_s, new_s);
            for line in diff.lines() {
                eprintln!("{line}");
            }
            eprintln!();
        }
    }

    // Show diff preview for write_file (overwrite case)
    if tool_name == "write_file" {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            if let (Ok(old_content), Some(new_content)) = (
                std::fs::read_to_string(path),
                args.get("content").and_then(|v| v.as_str()),
            ) {
                let diff = diff_preview::preview_file_edit(path, &old_content, new_content);
                for line in diff.lines() {
                    eprintln!("{line}");
                }
                eprintln!();
            }
        }
    }

    eprintln!("  {}", ui::command("Approve?"));
    eprintln!("    1. Yes");
    eprintln!("    2. No");
    eprintln!("  {}", ui::dim("Use ↑/↓, Enter, or press 1/2."));

    match prompt_approval_choice() {
        Ok(true) => BeforeToolCallAction::Proceed,
        Ok(false) => BeforeToolCallAction::Deny(format!("User denied tool '{tool_name}'")),
        Err(err) => BeforeToolCallAction::Deny(format!("Failed to read approval input: {err}")),
    }
}

fn prompt_approval_choice() -> io::Result<bool> {
    // Run the raw-mode prompt on a dedicated OS thread to avoid blocking
    // the tokio runtime and to isolate terminal state from rustyline.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(prompt_approval_inner());
    });
    rx.recv()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
}

fn prompt_approval_inner() -> io::Result<bool> {
    struct RawModeGuard;

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }

    let mut selected = 0usize;
    enable_raw_mode()?;
    let _raw_mode_guard = RawModeGuard;

    loop {
        render_approval_menu(selected)?;

        // Timeout so we don't hang forever if the terminal can't deliver events
        if !event::poll(std::time::Duration::from_secs(60))? {
            // Timed out — fall back to deny
            eprintln!("\r\x1b[2K  {}", ui::dim("(timed out, denying)"));
            return Ok(false);
        }

        if let Event::Key(key) = event::read()? {
            // Accept Press and Repeat events; skip Release to avoid double-firing.
            if key.kind == KeyEventKind::Release {
                continue;
            }

            match key.code {
                KeyCode::Enter => return Ok(selected == 0),
                KeyCode::Up => selected = 0,
                KeyCode::Down => selected = 1,
                KeyCode::Char('1') => return Ok(true),
                KeyCode::Char('2') => return Ok(false),
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
                KeyCode::Char('n') | KeyCode::Char('N') => return Ok(false),
                _ => {}
            }
        }
    }
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
