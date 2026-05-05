use std::cmp;

const MAX_PREVIEW_LINES: usize = 80;
const MAX_LINE_LENGTH: usize = 200;

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

    if prefix > 0 {
        output.push(format!("  @@ unchanged prefix: {} lines @@", prefix));
    }

    if old_suffix > prefix {
        for line in &old_lines[prefix..old_suffix] {
            output.push(format!("- {}", truncate_line(line)));
        }
    }

    if new_suffix > prefix {
        for line in &new_lines[prefix..new_suffix] {
            output.push(format!("+ {}", truncate_line(line)));
        }
    }

    let unchanged_suffix = old_lines.len().saturating_sub(old_suffix);
    if unchanged_suffix > 0 {
        output.push(format!("  @@ unchanged suffix: {} lines @@", unchanged_suffix));
    }

    if output.len() > MAX_PREVIEW_LINES {
        let omitted = output.len() - MAX_PREVIEW_LINES;
        output.truncate(MAX_PREVIEW_LINES);
        output.push(format!("  ... diff truncated ({} more lines)", omitted));
    }

    output.join("\n")
}

pub fn colorize_diff(diff: &str) -> String {
    diff.lines()
        .map(|line| {
            if line.starts_with("+++") {
                format!("\x1b[32m{}\x1b[0m", line)
            } else if line.starts_with("---") {
                format!("\x1b[31m{}\x1b[0m", line)
            } else if line.starts_with("+") {
                format!("\x1b[32m{}\x1b[0m", line)
            } else if line.starts_with("-") {
                format!("\x1b[31m{}\x1b[0m", line)
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
}
