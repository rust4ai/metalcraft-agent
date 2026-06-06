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
            "--event-port" => {
                config.event_port = args.next().ok_or("--event-port requires a value")?.parse()?;
            }
            "--event-host" => {
                config.event_host = args.next().ok_or("--event-host requires a value")?;
            }
            "--event-persona" => {
                config.event_persona = Some(args.next().ok_or("--event-persona requires a value")?);
            }
            "--events" => {
                let value = args.next().ok_or("--events requires a value")?;
                config.event_types = value.split(',').map(|s| s.trim().to_string()).collect();
            }
            "--platforms" => {
                let value = args.next().ok_or("--platforms requires a value")?;
                config.event_platforms = Some(value.split(',').map(|s| s.trim().to_string()).collect());
            }
            "--admin-user-ids" => {
                let value = args.next().ok_or("--admin-user-ids requires a value")?;
                config.admin_user_ids = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--api" => {
                config.workshop_api_key = Some(args.next().ok_or("--api requires a key")?);
            }
            "--api-port" => {
                config.workshop_api_port = args.next().ok_or("--api-port requires a value")?.parse()?;
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
         Event listener options (requires AGENT_GATEWAY_URL):\n  \
           --event-port <n>         Webhook listener port (default: 3001)\n  \
           --event-host <host>      Host for gateway callback URL (default: localhost)\n  \
           --event-persona <slug>   Persona for event tasks (default: same as --persona)\n  \
           --events <list>          Comma-separated event types (default: message_create)\n  \
           --platforms <list>       Comma-separated platforms (default: all)\n  \
           --admin-user-ids <list>  Comma-separated platform user IDs allowed to trigger the agent (required)\n\n\
         Workshop API options:\n  \
           --api <KEY>              Enable workshop admin API with Bearer KEY (env: WORKSHOP_API_KEY)\n  \
           --api-port <n>           Workshop API port (default: 3002, env: WORKSHOP_API_PORT or PORT)\n\n\
         Most flow/event options also have STARKBOT_* env equivalents (see daemon.rs).\n\n\
         Required env vars for event listener:\n  \
           AGENT_GATEWAY_URL        Gateway base URL\n  \
           AGENT_GATEWAY_API_KEY    Gateway auth token\n  \
           EVENTD_WEBHOOK_SECRET    Secret for authenticating inbound webhooks\n  \
           EVENTD_ADMIN_USER_IDS    Comma-separated admin user IDs (alternative to --admin-user-ids)"
    );
}
