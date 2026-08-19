fn main() {
    metalcraft_agent::seed::ensure_defaults();
    let path = std::env::args().nth(1).expect("path to .agentpack");
    let bytes = std::fs::read(&path).expect("read archive");
    match metalcraft_agent::agent_packs::install(&bytes, "roundtrip") {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        Err(e) => {
            eprintln!("install failed: {e}");
            std::process::exit(1);
        }
    }
}
