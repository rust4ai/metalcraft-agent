//! Build a `.agentpack` archive from a pack directory.
//!
//! The packs under `unbundled_packs/` ship in this repo but not in the binary,
//! so the only way to get one onto a pod is to publish it to a registry — and
//! that starts with a real archive. Zipping the directory by hand does not
//! work: the manifest's `content_sha256` and consent summary are derived from
//! the bytes, and a pod rejects an archive whose manifest disagrees with what
//! it carries.
//!
//! ```sh
//! cargo run --example pack_dir -- unbundled_packs/email /tmp/email.agentpack
//! ```

fn main() {
    let dir = std::env::args().nth(1).expect("path to a pack directory");
    let out = std::env::args()
        .nth(2)
        .expect("path to write the .agentpack to");
    match metalcraft_agent::agent_packs::bundle::from_dir(std::path::Path::new(&dir)) {
        Ok(bytes) => {
            std::fs::write(&out, &bytes).expect("write archive");
            println!("wrote {} bytes to {out}", bytes.len());
        }
        Err(e) => {
            eprintln!("packing {dir} failed: {e}");
            std::process::exit(1);
        }
    }
}
