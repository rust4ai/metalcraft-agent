fn main() {
    metalcraft_agent::seed::ensure_defaults();
    let slug = std::env::args().nth(1).expect("slug");
    let version = std::env::args().nth(2).unwrap_or_else(|| "1.0.0".into());
    let out = std::env::args().nth(3).expect("out path");
    match metalcraft_agent::agent_packs::export(&slug, &version) {
        Ok(bytes) => {
            std::fs::write(&out, &bytes).unwrap();
            println!("wrote {} bytes to {out}", bytes.len());
        }
        Err(e) => {
            eprintln!("export failed: {e}");
            std::process::exit(1);
        }
    }
}
