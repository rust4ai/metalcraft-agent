//! CLI wrapper around [`metalcraft_agent::daemon`]. Parses flags on top of the
//! env-derived [`DaemonConfig`] defaults, then delegates to `daemon::run`. The
//! actual daemon logic lives in the library so the `starkbot-metal` umbrella
//! crate can run it via `daemon::run_daemon()`.

use metalcraft_agent::daemon::{self, DaemonConfig};

use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env FIRST so a file-provided METALCRAFT_DATA_DIR is honored by
    // seeding (which resolves the data dir) and by RUST_LOG below. Otherwise
    // seeding lands in the fallback dir while the runtime reads the override.
    dotenvy::dotenv().ok();
    env_logger::init();

    metalcraft_agent::seed::ensure_defaults();

    // Start from environment defaults, then let CLI flags override.
    let mut config = DaemonConfig::from_env();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--flows-dir" => {
                config.flows_dir = PathBuf::from(args.next().ok_or("--flows-dir requires a path")?);
            }
            "--persona" => {
                config.persona_slug = args.next().ok_or("--persona requires a value")?;
            }
            "--model" => {
                config.model_name = args.next().ok_or("--model requires a value")?;
            }
            "--poll-seconds" => {
                config.poll_seconds = args.next().ok_or("--poll-seconds requires a value")?.parse()?;
            }
            "--once" => {
                config.once = true;
            }
            "--auto-approve" => {
                config.auto_approve = true;
            }
            "--api" => {
                config.workshop_api_key = Some(args.next().ok_or("--api requires a key")?);
            }
            "--api-oidc" => {
                // OIDC-only: serve the workshop API with no static key, accepting
                // only Metalcraft ID (`mck_`) tokens. Same as WORKSHOP_API_ENABLED=1.
                config.workshop_api_oidc = true;
            }
            "--api-port" => {
                config.workshop_api_port = args.next().ok_or("--api-port requires a value")?.parse()?;
            }
            // Retired external-gateway flags. Accepted and ignored (each consumed
            // a value) so older deploy start commands don't crash the daemon on
            // upgrade — the external gateway was replaced by gateway channels.
            "--event-port" | "--event-host" | "--event-persona" | "--events"
            | "--platforms" | "--admin-user-ids" => {
                let _ = args.next();
                eprintln!(
                    "warning: '{arg}' is deprecated and ignored (the external gateway was \
                     removed; configure gateway channels in the workshop instead)"
                );
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => {
                return Err(format!("unknown argument: {other}").into());
            }
        }
    }

    daemon::run(config).await
}

fn print_usage() {
    println!(
        "metalcraft-daemon [OPTIONS]\n\n\
         Flow options:\n  \
           --flows-dir <path>       Flows directory\n  \
           --persona <slug>         Persona for flow tasks (default: coding-agent)\n  \
           --model <name>           LLM model name\n  \
           --poll-seconds <n>       Poll interval (default: 30)\n  \
           --once                   Run once and exit\n  \
           --auto-approve           Skip tool approval prompts\n\n\
         Workshop API options:\n  \
           --api <KEY>              Enable workshop admin API with Bearer KEY (env: WORKSHOP_API_KEY)\n  \
           --api-oidc               Enable workshop admin API with OIDC-only auth, no static key\n  \
           \x20                        (env: WORKSHOP_API_ENABLED=1; managed pods)\n  \
           --api-port <n>           Workshop API port (default: 3002, env: WORKSHOP_API_PORT or PORT)\n\n\
         The workshop API also hosts gateway channels (inbound webhooks at\n  \
         /webhook/<adapter> and management under /api/v1/gateway/*).\n\n\
         Most flow options also have STARKBOT_* env equivalents (see daemon.rs)."
    );
}
