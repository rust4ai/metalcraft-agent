//! Dump the workshop API's OpenAPI document to stdout.
//!
//! This is the canonical way to regenerate the committed `openapi.json` that the
//! workshop clients generate their TypeScript types from:
//!
//! ```sh
//! cargo run --example dump_openapi > openapi.json
//! ```
//!
//! The running pod also serves the same document live at `GET /api/v1/openapi.json`.

use utoipa::OpenApi;

fn main() {
    let doc = metalcraft_agent::workshop_api::ApiDoc::openapi();
    println!("{}", doc.to_pretty_json().expect("serialize OpenAPI"));
}
