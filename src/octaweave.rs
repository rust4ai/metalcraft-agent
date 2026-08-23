//! Octaweave — the workspace an agent works inside.
//!
//! [octaweave.com](https://octaweave.com) is notes, a board, a drive, a calendar and a
//! blog, and the `octaweave` integration pack turns that API into 32 typed tools. What
//! this module answers is the question a person has *before* any of that: **can this
//! pod reach my workspace, and if not, what do I press?**
//!
//! ## Why there is no key to paste
//!
//! The pack asks for an `owk_` workspace key. Octaweave also accepts an `mck_` account
//! token, which names the human and carries exactly their existing memberships
//! (`octaweave/docs/ECOSYSTEM_PIVOT_PLAN.md` §3.1) — and every managed pod already
//! holds one. So the credential is not minted, pasted, or stored here: the pod presents
//! the identity it was born with, and [`crate::tools::http_api`] stands it in for
//! `$OCTAWEAVE_API_KEY` **only** on requests bound for Octaweave itself.
//!
//! What a human still has to do once is link their Metalcraft account to their
//! Octaweave one. That is a browser trip to Octaweave's own account page, which is why
//! the interesting answer here is [`OctaweaveConnectionState::Unlinked`] and the URL beside it.
use serde::{Deserialize, Serialize};

/// Where Octaweave lives. Overridable through the key store for a self-hosted or local
/// instance — the same escape hatch every other host in this pod gets.
pub const DEFAULT_BASE_URL: &str = "https://octaweave.com";

/// The key-store entry holding this pod's Metalcraft ID token.
const POD_TOKEN_KEY: &str = "METALCRAFT_TOKEN";

/// The integration pack that provides the tools.
pub const PACK_ID: &str = "octaweave";

pub fn base_url() -> String {
    crate::key_store::lookup_present("OCTAWEAVE_BASE_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// How far this pod got with Octaweave, in the terms a panel can act on.
///
/// Named for its service rather than `ConnectionState`, because `utoipa` keys the
/// OpenAPI document by type name: a second `Connection` does not conflict loudly, it
/// **silently replaces** the first, and the generated client then types one endpoint
/// against another endpoint's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OctaweaveConnectionState {
    /// Octaweave resolved this pod's token to an account there.
    Connected,
    /// The credential reached Octaweave and no account there claims it. The one state
    /// a person fixes in one click, which is why it is not folded into the next one.
    Unlinked,
    /// This pod holds no Metalcraft token, so there is nothing to present.
    NoToken,
    /// Octaweave is not answering, or answered something this pod cannot read.
    Unreachable,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct OctaweaveConnection {
    pub url: String,
    pub state: OctaweaveConnectionState,
    /// Whatever Octaweave will say about the account — an email or a name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Where a human goes to finish it. Octaweave's own answer when it offers one,
    /// falling back to its account page — the page that hosts the link button.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    /// The host's own words when something went wrong, shown rather than paraphrased.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the tools are actually installed. Connecting without the pack reaches
    /// nothing, and installing the pack without connecting authenticates as nobody —
    /// a panel needs both facts to say which half is missing.
    pub pack_installed: bool,
}

/// Ask Octaweave who this pod is.
///
/// Every failure is an [`OctaweaveConnection`] rather than an `Err`, for the same reason the
/// registry probe is: "not linked yet" and "the host is down" are things a panel
/// renders, and collapsing them into an error would leave it unable to tell them apart.
pub async fn status() -> OctaweaveConnection {
    let url = base_url();
    let pack_installed = crate::integrations::find_installed(PACK_ID).is_some();
    let mut conn = OctaweaveConnection {
        url: url.clone(),
        state: OctaweaveConnectionState::NoToken,
        account: None,
        link_url: None,
        detail: None,
        pack_installed,
    };

    let Some(token) = crate::key_store::lookup_present(POD_TOKEN_KEY) else {
        return conn;
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        // A redirect is how an allowed origin gets used to reach one that isn't, and
        // this request carries the pod's account token.
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            conn.state = OctaweaveConnectionState::Unreachable;
            conn.detail = Some(e.to_string());
            return conn;
        }
    };

    let resp = client
        .get(format!("{url}/api/v1/whoami"))
        .bearer_auth(token)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            conn.state = OctaweaveConnectionState::Unreachable;
            conn.detail = Some(e.to_string());
            return conn;
        }
    };

    let code = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    conn.detail = body.get("error").and_then(|v| v.as_str()).map(str::to_string);
    conn.account = ["email", "name", "handle"]
        .iter()
        .find_map(|k| body.get(*k).and_then(|v| v.as_str()))
        .map(str::to_string);

    conn.state = match code {
        c if c.is_success() => OctaweaveConnectionState::Connected,
        // Octaweave answers a plain 401 for a token whose account is not linked —
        // deliberately, since saying more would confirm which tokens exist. That makes
        // "not linked" and "token no good" indistinguishable from here, and of the two
        // readings only one has an action attached, so we offer it: the link page. The
        // host's own message rides along in `detail` for the case where it is the other.
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            conn.link_url = Some(
                body.get("link_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{url}/account")),
            );
            OctaweaveConnectionState::Unlinked
        }
        c => {
            conn.detail
                .get_or_insert_with(|| format!("octaweave answered {c}"));
            OctaweaveConnectionState::Unreachable
        }
    };
    conn
}
