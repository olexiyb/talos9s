//! Is a newer talos9s released? Asks GitHub for the latest release, at most
//! once a day (the answer is kept in the state file), in the background with
//! a short time limit. It is the only connection talos9s makes outside the
//! clusters; `--no-update-check`, `TALOS9S_NO_UPDATE_CHECK` or the common
//! `DO_NOT_TRACK=1` turn it off.

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use std::time::Duration;

/// The running version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where releases are listed, for people to read
pub const RELEASES_URL: &str = "https://github.com/olexiyb/talos9s/releases/latest";

const LATEST_API: &str = "https://api.github.com/repos/olexiyb/talos9s/releases/latest";

/// Between two checks
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Upper bound for the whole request
const TIMEOUT: Duration = Duration::from_secs(5);

/// `DO_NOT_TRACK` set to anything but empty, `0` or `false`
/// (<https://consoledonottrack.com>)
pub fn do_not_track() -> bool {
    std::env::var("DO_NOT_TRACK")
        .is_ok_and(|v| !matches!(v.trim().to_lowercase().as_str(), "" | "0" | "false"))
}

/// The latest release's version (without the `v`)
pub async fn fetch_latest() -> Result<String, String> {
    tokio::time::timeout(TIMEOUT, request())
        .await
        .map_err(|_| format!("no answer within {}s", TIMEOUT.as_secs()))?
}

async fn request() -> Result<String, String> {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .map_err(|e| e.to_string())?
        .https_only()
        .enable_http1()
        .build();
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build::<_, Empty<Bytes>>(https);
    let request = hyper::Request::get(LATEST_API)
        // GitHub refuses requests without one
        .header("user-agent", format!("talos9s/{VERSION}"))
        .header("accept", "application/vnd.github+json")
        .body(Empty::new())
        .map_err(|e| e.to_string())?;
    let response = client.request(request).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("GitHub answered {}", response.status()));
    }
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    json.get("tag_name")
        .and_then(|t| t.as_str())
        .map(|t| t.trim_start_matches('v').to_string())
        .ok_or_else(|| "no tag_name in the answer".to_string())
}

pub use talos9s_core::version::is_newer;
