//! Portable export/import — the **Obsidian-compatible** format from the cloud
//! `metalcraft-notes` (`services/portable.rs`): one flat `.md` per note, YAML
//! frontmatter for metadata, body verbatim. Round-trip fields (`slug`) live
//! under a `metalcraft:` namespace foreign vaults ignore.
//!
//! Unlike the cloud, we **don't** pull in `serde_yaml` (unmaintained): the
//! frontmatter is a fixed, small shape, so a hand-rolled emitter/parser covers
//! our own exports and tolerates foreign frontmatter (unknown keys ignored).

use std::io::Write;

/// Parsed frontmatter (only the fields we care about; others are ignored).
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub title: String,
    pub favorite: bool,
    pub categories: Vec<String>,
    /// Our round-trip slug (`metalcraft.slug`); None for foreign vaults.
    pub slug: Option<String>,
}

/// A note to create on import.
#[derive(Debug, Clone)]
pub struct ImportNode {
    pub title: String,
    pub body: String,
    pub favorite: bool,
    pub categories: Vec<String>,
    pub preferred_slug: Option<String>,
}

fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn yaml_unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Serialize one note (+ its category names) to `---\n<yaml>---\n\n<body>\n`.
/// `created`/`updated` are ISO strings written verbatim (round-trip only).
pub fn serialize_note(
    title: &str,
    slug: &str,
    body: &str,
    favorite: bool,
    created: &str,
    updated: &str,
    categories: &[String],
) -> String {
    let mut yaml = String::new();
    yaml.push_str(&format!("title: {}\n", yaml_quote(title)));
    if !created.is_empty() {
        yaml.push_str(&format!("created: {}\n", yaml_quote(created)));
    }
    if !updated.is_empty() {
        yaml.push_str(&format!("updated: {}\n", yaml_quote(updated)));
    }
    if favorite {
        yaml.push_str("favorite: true\n");
    }
    if !categories.is_empty() {
        yaml.push_str("categories:\n");
        for c in categories {
            yaml.push_str(&format!("  - {}\n", yaml_quote(c)));
        }
    }
    yaml.push_str("metalcraft:\n");
    yaml.push_str(&format!("  slug: {}\n", yaml_quote(slug)));

    let body = body.trim_end_matches('\n');
    format!("---\n{yaml}---\n\n{body}\n")
}

/// Split raw file contents into (frontmatter, body). `None` frontmatter when
/// there's no leading `---` block (a foreign note).
pub fn split_frontmatter(raw: &str) -> (Option<Frontmatter>, String) {
    let rest = match raw.strip_prefix("---\n").or_else(|| raw.strip_prefix("---\r\n")) {
        Some(r) => r,
        None => return (None, raw.to_string()),
    };
    let mut yaml = String::new();
    let mut lines = rest.lines();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return (None, raw.to_string());
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    let body = body.trim_start_matches('\n').to_string();
    (Some(parse_frontmatter(&yaml)), body)
}

enum Ctx {
    None,
    Categories,
    Metalcraft,
}

/// Minimal frontmatter parser for our known keys (title, favorite, categories,
/// metalcraft.slug). Tolerant: unknown keys and foreign structure are skipped.
fn parse_frontmatter(yaml: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    let mut ctx = Ctx::None;
    for line in yaml.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        let indented = trimmed.len() < line.len();
        if indented {
            match ctx {
                Ctx::Categories if trimmed.starts_with("- ") => {
                    let v = yaml_unquote(&trimmed[2..]);
                    if !v.is_empty() {
                        fm.categories.push(v);
                    }
                }
                Ctx::Metalcraft => {
                    if let Some(rest) = trimmed.strip_prefix("slug:") {
                        fm.slug = Some(yaml_unquote(rest));
                    }
                }
                _ => {}
            }
            continue;
        }
        ctx = Ctx::None;
        let Some((key, val)) = line.split_once(':') else { continue };
        let (key, val) = (key.trim(), val.trim());
        match key {
            "title" => fm.title = yaml_unquote(val),
            "favorite" => fm.favorite = val == "true",
            "metalcraft" => ctx = Ctx::Metalcraft,
            "categories" => {
                if val.is_empty() {
                    ctx = Ctx::Categories;
                } else if val.starts_with('[') {
                    let inner = val.trim_start_matches('[').trim_end_matches(']');
                    for item in inner.split(',') {
                        let v = yaml_unquote(item);
                        if !v.is_empty() {
                            fm.categories.push(v);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    fm
}

/// Strip a markdown extension, yielding the base name; `None` if not markdown.
fn md_name(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    let last = norm.rsplit('/').next().unwrap_or(&norm);
    let lower = last.to_lowercase();
    let stem = if lower.ends_with(".md") {
        &last[..last.len() - 3]
    } else if lower.ends_with(".markdown") {
        &last[..last.len() - 9]
    } else {
        return None;
    };
    let stem = stem.trim();
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// Map notes (+ tags) into flat `(filename, contents)` pairs, one `.md` per note.
pub fn note_files(items: &[(String, String, String, bool, String, String, Vec<String>)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(title, slug, body, fav, created, updated, cats)| {
            (
                format!("{slug}.md"),
                serialize_note(title, slug, body, *fav, created, updated, cats),
            )
        })
        .collect()
}

/// Parse `(path, contents)` entries into flat import nodes (folders ignored).
pub fn parse_import(entries: Vec<(String, String)>) -> Vec<ImportNode> {
    let mut out = Vec::new();
    for (path, contents) in entries {
        let Some(name) = md_name(&path) else { continue };
        let (fm, body) = split_frontmatter(&contents);
        let title = fm
            .as_ref()
            .map(|f| f.title.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or(name);
        let favorite = fm.as_ref().map(|f| f.favorite).unwrap_or(false);
        let categories = fm.as_ref().map(|f| f.categories.clone()).unwrap_or_default();
        let preferred_slug = fm.as_ref().and_then(|f| f.slug.clone());
        out.push(ImportNode { title, body, favorite, categories, preferred_slug });
    }
    out
}

/// Read markdown entries from an uploaded zip, skipping dirs/dotfiles/non-md.
pub fn unzip_markdown(data: &[u8]) -> Result<Vec<(String, String)>, String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))
        .map_err(|e| format!("invalid zip: {e}"))?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("bad zip entry: {e}"))?;
        if !f.is_file() {
            continue;
        }
        let name = f.name().to_string();
        if name.split(['/', '\\']).any(|s| s.starts_with('.')) || name.starts_with("__MACOSX") {
            continue;
        }
        if md_name(&name).is_none() {
            continue;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_ok() {
            out.push((name, buf));
        }
    }
    Ok(out)
}

/// Package `(path, contents)` pairs into an in-memory zip.
pub fn zip_files(files: &[(String, String)]) -> std::io::Result<Vec<u8>> {
    use zip::write::{SimpleFileOptions, ZipWriter};
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for (path, contents) in files {
            zip.start_file(path.as_str(), opts)?;
            zip.write_all(contents.as_bytes())?;
        }
        zip.finish()?;
    }
    Ok(cursor.into_inner())
}

/// Turn an upload into markdown entries: a `.zip` is unpacked; a bare `.md`
/// becomes one note.
pub fn markdown_entries(filename: &str, data: &[u8]) -> Result<Vec<(String, String)>, String> {
    let is_zip = filename.to_lowercase().ends_with(".zip") || data.starts_with(b"PK\x03\x04");
    if is_zip {
        unzip_markdown(data)
    } else {
        let content = String::from_utf8_lossy(data).into_owned();
        let stem = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
        let stem = stem.strip_suffix(".md").or_else(|| stem.strip_suffix(".markdown")).unwrap_or(stem);
        let name = if stem.trim().is_empty() { "imported" } else { stem };
        Ok(vec![(format!("{name}.md"), content)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_round_trips() {
        let md = serialize_note(
            "Weekly: notes & \"things\"",
            "weekly",
            "# Hi\n\nbody",
            true,
            "2026-08-12T00:00:00.000Z",
            "2026-08-12T01:00:00.000Z",
            &["work".to_string(), "home".to_string()],
        );
        assert!(md.starts_with("---\n"));
        let (fm, body) = split_frontmatter(&md);
        let fm = fm.expect("frontmatter parses");
        assert_eq!(fm.title, "Weekly: notes & \"things\""); // quotes + colon survive
        assert!(fm.favorite);
        assert_eq!(fm.categories, vec!["work", "home"]);
        assert_eq!(fm.slug.as_deref(), Some("weekly"));
        assert_eq!(body, "# Hi\n\nbody");
    }

    #[test]
    fn foreign_note_without_frontmatter() {
        let (fm, body) = split_frontmatter("# Just markdown\n\nno frontmatter");
        assert!(fm.is_none());
        assert_eq!(body, "# Just markdown\n\nno frontmatter");
    }

    #[test]
    fn inline_categories_and_folders_ignored() {
        let entries = vec![
            ("Projects/foreign.md".to_string(), "---\ntitle: Foreign\ncategories: [a, b]\n---\n\nx".to_string()),
            ("loose.md".to_string(), "# Loose".to_string()),
        ];
        let nodes = parse_import(entries);
        assert_eq!(nodes.len(), 2);
        let f = nodes.iter().find(|n| n.title == "Foreign").unwrap();
        assert_eq!(f.categories, vec!["a", "b"]);
        assert!(nodes.iter().any(|n| n.title == "loose"));
    }

    #[test]
    fn export_import_preserves_fields() {
        let files = note_files(&[(
            "The Parent".to_string(),
            "parent".to_string(),
            "parent body".to_string(),
            true,
            "2026-08-12T00:00:00.000Z".to_string(),
            "2026-08-12T00:00:00.000Z".to_string(),
            vec!["personal".to_string()],
        )]);
        assert_eq!(files[0].0, "parent.md");
        let nodes = parse_import(files);
        let p = &nodes[0];
        assert_eq!(p.title, "The Parent");
        assert!(p.favorite);
        assert_eq!(p.preferred_slug.as_deref(), Some("parent"));
        assert_eq!(p.body, "parent body");
        assert_eq!(p.categories, vec!["personal"]);
    }

    #[test]
    fn zip_round_trips_markdown() {
        let files = vec![("a.md".to_string(), "# A".to_string()), ("b.md".to_string(), "# B".to_string())];
        let bytes = zip_files(&files).unwrap();
        let back = unzip_markdown(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back.iter().any(|(p, c)| p == "a.md" && c == "# A"));
    }
}
