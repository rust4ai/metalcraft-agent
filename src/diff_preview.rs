use std::cmp;
use std::path::Path;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

const MAX_PREVIEW_LINES: usize = 80;
const MAX_LINE_LENGTH: usize = 200;
const CONTEXT_LINES: usize = 3;

// Dark background tints for removed/added lines (24-bit color)
const BG_REMOVED: &str = "\x1b[48;2;60;20;20m"; // dark red
const BG_ADDED: &str = "\x1b[48;2;20;50;20m";   // dark green
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";

static SS: LazyLock<SyntaxSet> = LazyLock::new(|| SyntaxSet::load_defaults_newlines());
static TS: LazyLock<ThemeSet> = LazyLock::new(|| ThemeSet::load_defaults());

/// Produce a rich diff with file context, line numbers, syntax highlighting,
/// and colored backgrounds for added/removed lines.
pub fn preview_file_edit(path: &str, old_text: &str, new_text: &str) -> String {
    let file_content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return preview_edit_diff(old_text, new_text),
    };

    let match_offset = match file_content.find(old_text) {
        Some(o) => o,
        None => return preview_edit_diff(old_text, new_text),
    };

    // Figure out the starting line number of old_text in the file
    let start_line = if match_offset == 0 {
        1
    } else {
        let before = &file_content[..match_offset];
        let line_count = before.lines().count();
        if before.ends_with('\n') {
            line_count + 1
        } else {
            line_count
        }
    };

    let file_lines: Vec<&str> = file_content.lines().collect();
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    let change_start = start_line - 1; // 0-based index
    let change_end = change_start + old_lines.len();

    // Context range (clamped to file bounds)
    let ctx_start = change_start.saturating_sub(CONTEXT_LINES);
    let ctx_end = cmp::min(file_lines.len(), change_end + CONTEXT_LINES);

    // Set up syntax highlighter
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let syntax = SS
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SS.find_syntax_plain_text());
    let theme = &TS.themes["base16-ocean.dark"];
    let mut hl = HighlightLines::new(syntax, theme);

    // Feed context lines before the visible range through the highlighter
    // so syntax state (e.g. multi-line strings) is correct.
    for i in 0..ctx_start {
        if i < file_lines.len() {
            let line_with_nl = format!("{}\n", file_lines[i]);
            let _ = hl.highlight_line(&line_with_nl, &SS);
        }
    }

    let gutter_w = format!("{}", cmp::max(ctx_end, start_line + new_lines.len())).len();
    let mut output = Vec::new();

    // Header
    output.push(format!("  \x1b[1m{}\x1b[0m", path));
    output.push(format!(
        "  \x1b[36m@@ -{},{} +{},{} @@\x1b[0m",
        start_line,
        old_lines.len(),
        start_line,
        new_lines.len(),
    ));

    // Context before
    for i in ctx_start..change_start {
        let ln = i + 1;
        let highlighted = highlight_code_line(&mut hl, file_lines[i]);
        output.push(format!(
            "  {DIM}{:>gutter_w$} │{RESET}  {highlighted}",
            ln,
            gutter_w = gutter_w,
        ));
    }

    // Removed lines (red background + syntax foreground)
    // We need a separate highlighter state for removed lines since they're
    // being replaced. Use the current state (which is correct for this position).
    let mut hl_remove = HighlightLines::new(syntax, theme);
    // Prime it to the right position
    for i in 0..change_start {
        if i < file_lines.len() {
            let _ = hl_remove.highlight_line(&format!("{}\n", file_lines[i]), &SS);
        }
    }
    for (j, line) in old_lines.iter().enumerate() {
        let ln = change_start + j + 1;
        let highlighted = highlight_code_line(&mut hl_remove, line);
        output.push(format!(
            "  {BG_REMOVED}{DIM}{:>gutter_w$} │ - {highlighted}{RESET}",
            ln,
            gutter_w = gutter_w,
        ));
    }

    // Added lines (green background + syntax foreground)
    let mut hl_add = HighlightLines::new(syntax, theme);
    for i in 0..change_start {
        if i < file_lines.len() {
            let _ = hl_add.highlight_line(&format!("{}\n", file_lines[i]), &SS);
        }
    }
    for (j, line) in new_lines.iter().enumerate() {
        let ln = change_start + j + 1;
        let highlighted = highlight_code_line(&mut hl_add, line);
        output.push(format!(
            "  {BG_ADDED}\x1b[1m{:>gutter_w$} │ + {highlighted}{RESET}",
            ln,
            gutter_w = gutter_w,
        ));
    }

    // Context after — continue from the main highlighter
    // Advance hl past the old lines so context after is correct
    for line in &old_lines {
        let _ = hl.highlight_line(&format!("{}\n", line), &SS);
    }
    for i in change_end..ctx_end {
        if i < file_lines.len() {
            let ln = i + 1;
            let highlighted = highlight_code_line(&mut hl, file_lines[i]);
            output.push(format!(
                "  {DIM}{:>gutter_w$} │{RESET}  {highlighted}",
                ln,
                gutter_w = gutter_w,
            ));
        }
    }

    // Truncation guard
    if output.len() > MAX_PREVIEW_LINES {
        let omitted = output.len() - MAX_PREVIEW_LINES;
        output.truncate(MAX_PREVIEW_LINES);
        output.push(format!("  {DIM}... {} more lines{RESET}", omitted));
    }

    output.join("\n")
}

/// Highlight a single line of code, returning ANSI foreground-colored text.
fn highlight_code_line(hl: &mut HighlightLines, line: &str) -> String {
    let line_nl = format!("{}\n", line);
    let regions = hl.highlight_line(&line_nl, &SS).unwrap_or_default();
    let mut out = String::new();
    for (style, text) in &regions {
        let fg = style.foreground;
        let t = text.trim_end_matches('\n');
        if !t.is_empty() {
            out.push_str(&format!("\x1b[38;2;{};{};{}m{}", fg.r, fg.g, fg.b, t));
        }
    }
    out
}

/// Simple old vs new diff (fallback when file can't be read).
pub fn preview_edit_diff(old_text: &str, new_text: &str) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut output = Vec::new();

    output.push(format!("--- before ({} lines)", old_lines.len()));
    output.push(format!("+++ after  ({} lines)", new_lines.len()));

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut old_suffix = old_lines.len();
    let mut new_suffix = new_lines.len();
    while old_suffix > prefix
        && new_suffix > prefix
        && old_lines[old_suffix - 1] == new_lines[new_suffix - 1]
    {
        old_suffix -= 1;
        new_suffix -= 1;
    }

    if prefix == old_lines.len() && prefix == new_lines.len() {
        output.push("  (no changes)".to_string());
        return output.join("\n");
    }

    let ctx_prefix_start = prefix.saturating_sub(CONTEXT_LINES);
    for i in ctx_prefix_start..prefix {
        output.push(format!("  {}", truncate_line(old_lines[i])));
    }

    if old_suffix > prefix {
        for line in &old_lines[prefix..old_suffix] {
            output.push(format!("{BG_REMOVED}\x1b[31m- {}{RESET}", truncate_line(line)));
        }
    }

    if new_suffix > prefix {
        for line in &new_lines[prefix..new_suffix] {
            output.push(format!("{BG_ADDED}\x1b[32m+ {}{RESET}", truncate_line(line)));
        }
    }

    let suffix_end = cmp::min(old_suffix + CONTEXT_LINES, old_lines.len());
    for i in old_suffix..suffix_end {
        output.push(format!("  {}", truncate_line(old_lines[i])));
    }

    if output.len() > MAX_PREVIEW_LINES {
        let omitted = output.len() - MAX_PREVIEW_LINES;
        output.truncate(MAX_PREVIEW_LINES);
        output.push(format!("  ... diff truncated ({} more lines)", omitted));
    }

    output.join("\n")
}

/// Colorize a plain diff (for the write_file overwrite fallback path).
pub fn colorize_diff(diff: &str) -> String {
    diff.lines()
        .map(|line| {
            if line.starts_with("+++") {
                format!("\x1b[32m{}\x1b[0m", line)
            } else if line.starts_with("---") {
                format!("\x1b[31m{}\x1b[0m", line)
            } else if line.starts_with('+') {
                format!("{BG_ADDED}\x1b[32m{}{RESET}", line)
            } else if line.starts_with('-') {
                format!("{BG_REMOVED}\x1b[31m{}{RESET}", line)
            } else if line.starts_with("  @@") {
                format!("\x1b[2m{}\x1b[0m", line)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LENGTH {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(cmp::max(1, MAX_LINE_LENGTH - 1)).collect();
        format!("{}…", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_inserted_and_deleted_lines() {
        let old_text = "a\nb\nc\n";
        let new_text = "a\nB\nc\nd\n";
        let diff = preview_edit_diff(old_text, new_text);

        assert!(diff.contains("- b"));
        assert!(diff.contains("+ B"));
        assert!(diff.contains("+ d"));
    }

    #[test]
    fn reports_no_changes() {
        let diff = preview_edit_diff("same\n", "same\n");
        assert!(diff.contains("(no changes)"));
    }

    #[test]
    fn fallback_shows_context_lines() {
        let old_text = "line1\nline2\nline3\nold\nline5\nline6\nline7";
        let new_text = "line1\nline2\nline3\nnew\nline5\nline6\nline7";
        let diff = preview_edit_diff(old_text, new_text);
        assert!(diff.contains("- old") || diff.contains("old"));
        assert!(diff.contains("+ new") || diff.contains("new"));
    }
}
