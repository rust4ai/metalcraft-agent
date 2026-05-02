use metalcraft::BeforeToolCallAction;
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::Arc;

/// Build a `BeforeToolCallHook` for metalcraft based on approval mode.
pub fn build_hook(mode: ApprovalMode) -> Option<metalcraft::BeforeToolCallHook> {
    match mode {
        ApprovalMode::AutoApprove => None,
        ApprovalMode::Interactive { auto_approve_tools } => {
            Some(Arc::new(move |name: &str, args: &serde_json::Value| {
                if auto_approve_tools.contains(name) {
                    return BeforeToolCallAction::Proceed;
                }
                prompt_user(name, args)
            }))
        }
    }
}

#[derive(Clone)]
pub enum ApprovalMode {
    /// Always auto-approve everything.
    AutoApprove,
    /// Prompt for tools not in the auto-approve set.
    Interactive { auto_approve_tools: HashSet<String> },
}

impl ApprovalMode {
    /// Default policy: read-only tools auto-approve, everything else prompts.
    pub fn default_interactive() -> Self {
        let auto = ["read_file", "list_files", "grep", "find_files"]
            .into_iter()
            .map(String::from)
            .collect();
        Self::Interactive {
            auto_approve_tools: auto,
        }
    }
}

fn prompt_user(tool_name: &str, args: &serde_json::Value) -> BeforeToolCallAction {
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
    eprintln!("  \x1b[33m⚡ {}\x1b[0m {}", tool_name, args_display);
    eprint!("  Approve? [Y/n] ");
    io::stderr().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return BeforeToolCallAction::Deny("Failed to read input".into());
    }

    let answer = input.trim().to_lowercase();
    if matches!(answer.as_str(), "" | "y" | "yes") {
        BeforeToolCallAction::Proceed
    } else {
        BeforeToolCallAction::Deny(format!("User denied tool '{tool_name}'"))
    }
}
