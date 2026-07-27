//! Every bundled flow template (base + integration packs) must parse and pass
//! `metalcraft_flows::validate` — so shipped templates are never broken and stay
//! v2-conformant.

use metalcraft_flows::{validate, SavedFlow};
use std::path::{Path, PathBuf};

fn collect_templates(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_templates(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("json")
            && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some("flow_templates")
        {
            out.push(p);
        }
    }
}

#[test]
fn all_seed_flow_templates_parse_and_validate() {
    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed");
    let mut files = Vec::new();
    collect_templates(&seed, &mut files);
    assert!(!files.is_empty(), "found no flow templates under {}", seed.display());

    for f in &files {
        let raw = std::fs::read_to_string(f).unwrap();
        let flow: SavedFlow =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: parse error: {e}", f.display()));
        let errs = validate(&flow);
        assert!(errs.is_empty(), "{} failed validation: {:?}", f.display(), errs);
    }

    // No v1 templates should remain.
    for f in &files {
        let raw = std::fs::read_to_string(f).unwrap();
        let flow: SavedFlow = serde_json::from_str(&raw).unwrap();
        assert_eq!(flow.spec_version, "2", "{} is not spec_version 2", f.display());
    }
}
