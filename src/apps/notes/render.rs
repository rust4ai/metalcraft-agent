//! Markdown → sanitized HTML for the public note-share page. `render_markdown`
//! is ported verbatim from the cloud `metalcraft-notes` (`services/render.rs`);
//! `shared_page_html` from notes-r2's `store.rs`. Raw HTML in the source is never
//! passed through (`unsafe_ = false`) and the output is ammonia-sanitized.

use comrak::{markdown_to_html, ComrakOptions};

pub fn render_markdown(md: &str) -> String {
    let mut opts = ComrakOptions::default();
    opts.extension.table = true;
    opts.extension.tasklist = true;
    opts.extension.strikethrough = true;
    opts.extension.autolink = true;
    opts.render.unsafe_ = false;
    let html = markdown_to_html(md, &opts);
    ammonia::Builder::default()
        .add_tags(["input"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .clean(&html)
        .to_string()
}

/// Escape the HTML-significant chars for the page `<title>`/heading.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The standalone public share page (self-contained HTML + CSS).
pub fn shared_page_html(title: &str, body_md: &str) -> String {
    let content = render_markdown(body_md);
    let t = esc(title);
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>{t} · Metalcraft Notes</title>
<style>
 body {{ margin:0; background:#fafafa; color:#1a1a1a; font:17px/1.7 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif; }}
 .wrap {{ max-width:720px; margin:0 auto; padding:64px 24px; }}
 h1.doc {{ font-size:34px; letter-spacing:-.02em; margin:0 0 24px; }}
 .md h1,.md h2,.md h3 {{ letter-spacing:-.01em; margin-top:1.6em; }}
 .md pre {{ background:#0b0d10; color:#e7e9ea; padding:14px 16px; border-radius:10px; overflow:auto; }}
 .md code {{ font-family:ui-monospace,Menlo,monospace; font-size:.9em; }}
 .md :not(pre) > code {{ background:#eee; padding:2px 5px; border-radius:5px; }}
 .md table {{ border-collapse:collapse; }} .md th,.md td {{ border:1px solid #ddd; padding:6px 10px; }}
 .md blockquote {{ border-left:3px solid #ddd; margin:0; padding-left:16px; color:#555; }}
 .foot {{ margin-top:48px; padding-top:16px; border-top:1px solid #eee; color:#999; font-size:13px; }}
</style></head><body><div class="wrap">
<h1 class="doc">{t}</h1>
<div class="md">{content}</div>
<div class="foot">Shared via Metalcraft Notes</div>
</div></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_markdown_and_sanitizes() {
        let html = render_markdown("# Hi\n\n- a\n- b\n\n<script>alert(1)</script>");
        assert!(html.contains("<h1>Hi</h1>"));
        assert!(html.contains("<li>a</li>"));
        assert!(!html.contains("<script>")); // sanitized away
    }

    #[test]
    fn share_page_embeds_title_and_body() {
        let page = shared_page_html("My <b>Note</b>", "**bold**");
        assert!(page.contains("My &lt;b&gt;Note&lt;/b&gt;")); // title escaped
        assert!(page.contains("<strong>bold</strong>"));
    }
}
