//! `metalcraft-migrate` — move a user's app data from the cloud service into
//! their pod (Plan B, Phase 2), over the app's own export/import HTTP surface.
//!
//! Notes (and later calendar/drive) expose the *same* export/import contract in
//! the cloud and in the pod, so migration is: `GET {source}/export` →
//! `POST {target}/import`, then a **reconcile** (compare item counts). Import is
//! additive (slugs deduped), so re-running never clobbers; `--dry-run` exports +
//! counts without writing.
//!
//! It migrates **one user** (one source token + one target pod). The per-user
//! batch — enumerate premium users, resolve each pod URL + mint a token — is an
//! operator loop around this (see `docs/POD_NATIVE_APPS_B_IMPL_PLAN.md` Phase 2).
//!
//! Example (cloud notes → a pod):
//! ```text
//! metalcraft-migrate notes \
//!   --from https://notes.metalcraftai.com        --from-token mck_… \
//!   --to   https://andrew-ab12.pods.metalcraftai.com --to-token mck_…
//! ```
//! (`--from-mount`/`--to-mount` default to the cloud root and the pod
//! `/apps/metalcraft-notes` respectively; set both to the pod mount to test
//! pod→pod.)

use std::collections::HashMap;
use std::process::ExitCode;

use reqwest::multipart::{Form, Part};
use reqwest::Client;

struct Args {
    app: String,
    from: String,
    from_token: String,
    from_mount: String,
    to: String,
    to_token: String,
    to_mount: String,
    dry_run: bool,
    force: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: metalcraft-migrate <app> --from URL --from-token T --to URL --to-token T \\\n         \
         [--from-mount M] [--to-mount M] [--dry-run] [--force]\n\n  \
         app: notes (the only migratable app today)\n  \
         --from-mount default '' (cloud root); --to-mount default '/apps/metalcraft-<app>' (pod).\n  \
         --force: import even if the target is non-empty (default aborts to avoid duplicating)."
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let app = it.next().unwrap_or_default();
    if app.is_empty() || app.starts_with("--") {
        usage();
    }
    let mut m: HashMap<String, String> = HashMap::new();
    let mut dry_run = false;
    let mut force = false;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--dry-run" => dry_run = true,
            "--force" => force = true,
            "--from" | "--from-token" | "--from-mount" | "--to" | "--to-token" | "--to-mount" => {
                let Some(val) = it.next() else { usage() };
                m.insert(flag.trim_start_matches("--").to_string(), val);
            }
            _ => usage(),
        }
    }
    let need = |k: &str| m.get(k).cloned().unwrap_or_else(|| usage());
    Args {
        from: need("from").trim_end_matches('/').to_string(),
        from_token: need("from-token"),
        from_mount: m.get("from-mount").cloned().unwrap_or_default(),
        to: need("to").trim_end_matches('/').to_string(),
        to_token: need("to-token"),
        to_mount: m
            .get("to-mount")
            .cloned()
            .unwrap_or_else(|| format!("/apps/metalcraft-{app}")),
        dry_run,
        force,
        app,
    }
}

type DynErr = Box<dyn std::error::Error + Send + Sync>;

async fn count_notes(client: &Client, base: &str, mount: &str, token: &str) -> Result<usize, DynErr> {
    let url = format!("{base}{mount}/api/v1/notes");
    let resp = client.get(&url).bearer_auth(token).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("GET {url} → HTTP {}: {body}", status.as_u16()).into());
    }
    let arr: serde_json::Value = serde_json::from_str(&body)?;
    Ok(arr.as_array().map(|a| a.len()).unwrap_or(0))
}

async fn export_zip(client: &Client, base: &str, mount: &str, token: &str) -> Result<Vec<u8>, DynErr> {
    let url = format!("{base}{mount}/api/v1/export");
    let resp = client.get(&url).bearer_auth(token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("export GET {url} → HTTP {}: {body}", status.as_u16()).into());
    }
    Ok(resp.bytes().await?.to_vec())
}

async fn import_zip(
    client: &Client,
    base: &str,
    mount: &str,
    token: &str,
    zip: Vec<u8>,
) -> Result<usize, DynErr> {
    let url = format!("{base}{mount}/api/v1/import");
    let part = Part::bytes(zip)
        .file_name("export.zip")
        .mime_str("application/zip")?;
    let form = Form::new().part("file", part);
    let resp = client.post(&url).bearer_auth(token).multipart(form).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("import POST {url} → HTTP {}: {body}", status.as_u16()).into());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    Ok(v.get("imported").and_then(|n| n.as_u64()).unwrap_or(0) as usize)
}

async fn run(args: Args) -> Result<(), DynErr> {
    if args.app != "notes" {
        return Err(format!("unsupported app '{}' (only 'notes' today)", args.app).into());
    }
    let client = Client::builder()
        .user_agent("metalcraft-migrate")
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let source_count = count_notes(&client, &args.from, &args.from_mount, &args.from_token).await?;
    println!("source: {source_count} note(s) at {}{}", args.from, args.from_mount);

    let zip = export_zip(&client, &args.from, &args.from_mount, &args.from_token).await?;
    println!("exported: {} bytes", zip.len());

    if args.dry_run {
        println!("dry-run: skipping import.");
        return Ok(());
    }

    let before = count_notes(&client, &args.to, &args.to_mount, &args.to_token).await?;
    // Import is additive (never clobbers), so re-running duplicates. Guard the
    // common case — migrate into a fresh pod — and require --force otherwise.
    if before > 0 && !args.force {
        return Err(format!(
            "target already has {before} note(s); refusing to import (would duplicate). Pass --force to import anyway."
        )
        .into());
    }
    let imported = import_zip(&client, &args.to, &args.to_mount, &args.to_token, zip).await?;
    let after = count_notes(&client, &args.to, &args.to_mount, &args.to_token).await?;

    println!(
        "target: {before} → {after} note(s) at {}{} (imported {imported})",
        args.to, args.to_mount
    );

    // Reconcile: import is additive, so the target should grow by `imported`, and
    // `imported` should account for every source note (first run into an empty
    // target ⇒ after == source_count).
    let mut ok = true;
    if after - before != imported {
        eprintln!("WARN: target grew by {} but import reported {imported}", after - before);
        ok = false;
    }
    if imported < source_count {
        eprintln!("WARN: imported {imported} < source {source_count} (some notes not migrated)");
        ok = false;
    }
    if ok {
        println!("reconcile OK: {imported} migrated, no discrepancies.");
    } else {
        return Err("reconcile found discrepancies (see warnings above)".into());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = parse_args();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("migration failed: {e}");
            ExitCode::FAILURE
        }
    }
}
