//! Command-line parsing for the `metalcraft-agent` binary, factored out of
//! `main()` so persona/task/flag resolution is pure and unit-testable.
//!
//! Persona is a **flag** (`--persona/-p <slug>`), not a positional, so every
//! positional token is the one-shot task. This lets `metalcraft-agent "fix the
//! bug"` work (and lets Orca inject a task as argv) while the persona stays
//! optional, defaulting to the Orchestrator. Env fallbacks
//! (`METALCRAFT_PERSONA`, `WORKSHOP_API_KEY`, `WORKSHOP_API_PORT`) are applied
//! by `main()` on top of this argv-only result, keeping the parser free of
//! environment reads.

/// A parsed `metalcraft-agent` command line (argv only — env fallbacks are
/// layered on by the caller).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CliInvocation {
    /// `--auto-approve` was present.
    pub auto_approve: bool,
    /// Explicit `--persona/-p <slug>`; `None` => fall back to env/default.
    pub persona: Option<String>,
    /// Explicit `--preset <slug>`; `None` => fall back to `METALCRAFT_PRESET`, then
    /// the built-in `general-agent`. The preset decides the default persona and the
    /// roster that persona may delegate to; an explicit `--persona` still wins.
    pub preset: Option<String>,
    /// All positional tokens joined with spaces; `None` => interactive mode.
    pub task: Option<String>,
    /// `--api` was present (server mode requested even without an inline key).
    pub api_requested: bool,
    /// Inline `--api <KEY>` value, if any.
    pub api_key: Option<String>,
    /// `--api-port <n>` value, if any.
    pub api_port: Option<u16>,
}

/// Parse raw argv (already skipping argv[0]). Returns an error string for a
/// flag that's missing its required value or has a malformed one.
pub fn parse_cli_invocation(raw: &[String]) -> Result<CliInvocation, String> {
    let mut inv = CliInvocation::default();
    let mut task_words: Vec<String> = Vec::new();

    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        match arg {
            "--auto-approve" => inv.auto_approve = true,
            "--preset" => {
                i += 1;
                let slug = raw
                    .get(i)
                    .ok_or_else(|| "--preset requires an agent preset slug".to_string())?;
                inv.preset = Some(slug.clone());
            }
            "--persona" | "-p" => {
                i += 1;
                let slug = raw
                    .get(i)
                    .ok_or_else(|| format!("{arg} requires a persona slug"))?;
                inv.persona = Some(slug.clone());
            }
            "--api" => {
                inv.api_requested = true;
                // Optional inline key: consume the next token only if it isn't
                // another flag (so `--api --api-port 3003` still parses).
                if let Some(next) = raw.get(i + 1) {
                    if !next.starts_with('-') {
                        inv.api_key = Some(next.clone());
                        i += 1;
                    }
                }
            }
            "--api-port" => {
                i += 1;
                let val = raw
                    .get(i)
                    .ok_or_else(|| "--api-port requires a value".to_string())?;
                inv.api_port = Some(
                    val.parse::<u16>()
                        .map_err(|_| format!("invalid --api-port value: {val}"))?,
                );
            }
            // Everything else is part of the one-shot task.
            _ => task_words.push(raw[i].clone()),
        }
        i += 1;
    }

    if !task_words.is_empty() {
        inv.task = Some(task_words.join(" "));
    }
    Ok(inv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_task_is_the_task_not_a_persona() {
        let inv = parse_cli_invocation(&args(&["fix", "the", "bug"])).unwrap();
        assert_eq!(inv.task.as_deref(), Some("fix the bug"));
        assert_eq!(inv.persona, None);
        assert!(!inv.auto_approve);
    }

    #[test]
    fn persona_flag_then_task() {
        for flag in ["--persona", "-p"] {
            let inv = parse_cli_invocation(&args(&[flag, "coding-agent", "do", "x"])).unwrap();
            assert_eq!(inv.persona.as_deref(), Some("coding-agent"));
            assert_eq!(inv.task.as_deref(), Some("do x"));
        }
    }

    #[test]
    fn no_args_is_interactive() {
        let inv = parse_cli_invocation(&args(&[])).unwrap();
        assert_eq!(inv.task, None);
        assert_eq!(inv.persona, None);
    }

    #[test]
    fn auto_approve_in_any_position() {
        let inv = parse_cli_invocation(&args(&["--auto-approve", "ship", "it"])).unwrap();
        assert!(inv.auto_approve);
        assert_eq!(inv.task.as_deref(), Some("ship it"));

        let inv = parse_cli_invocation(&args(&["-p", "x", "ship", "--auto-approve"])).unwrap();
        assert!(inv.auto_approve);
        assert_eq!(inv.task.as_deref(), Some("ship"));
    }

    #[test]
    fn api_with_inline_key_and_port() {
        let inv = parse_cli_invocation(&args(&["--api", "secret", "--api-port", "3010"])).unwrap();
        assert!(inv.api_requested);
        assert_eq!(inv.api_key.as_deref(), Some("secret"));
        assert_eq!(inv.api_port, Some(3010));
        assert_eq!(inv.task, None);
    }

    #[test]
    fn api_flag_without_key_does_not_swallow_following_flag() {
        let inv = parse_cli_invocation(&args(&["--api", "--api-port", "3010"])).unwrap();
        assert!(inv.api_requested);
        assert_eq!(inv.api_key, None);
        assert_eq!(inv.api_port, Some(3010));
    }

    #[test]
    fn missing_persona_value_errors() {
        assert!(parse_cli_invocation(&args(&["--persona"])).is_err());
    }

    #[test]
    fn bad_port_errors() {
        assert!(parse_cli_invocation(&args(&["--api", "k", "--api-port", "nope"])).is_err());
    }
}
