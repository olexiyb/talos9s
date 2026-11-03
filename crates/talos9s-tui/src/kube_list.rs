//! Kubernetes list calls that ride out a busy API server
//!
//! The API server answers `429 TooManyRequests` while it can't serve a list
//! yet, e.g. "storage is (re)initializing" while its watch cache for a
//! resource rebuilds after a restart, and expects the client to retry.
//! kube-rs doesn't, so these calls retry a few times before giving up.

use k8s_openapi::serde::de::DeserializeOwned;
use kube::Api;
use kube::api::{ListParams, ObjectList};
use std::fmt::Debug;
use std::time::Duration;
use talos9s_core::constants::K8S_REQUEST_TIMEOUT;

/// A Kubernetes error for the screen: an API error as the API server's own
/// message (kube-rs's text adds the whole `Status` dump, several lines per
/// error), anything else as kube-rs says it
pub fn error_text(e: &kube::Error) -> String {
    match e {
        kube::Error::Api(status) if !status.message.is_empty() => status.message.clone(),
        kube::Error::Api(status) => format!("{} ({})", status.reason, status.code),
        other => other.to_string(),
    }
}

/// Waits before each retry of a 429
const BACKOFF: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// `api.list(params)`, retried while the API server answers 429, each try
/// bounded by [`K8S_REQUEST_TIMEOUT`]
pub async fn list<K>(api: &Api<K>, params: &ListParams) -> Result<ObjectList<K>, kube::Error>
where
    K: Clone + DeserializeOwned + Debug,
{
    let mut waits = BACKOFF.iter();
    loop {
        match bounded(api.list(params)).await {
            Err(e) if is_too_many_requests(&e) => match waits.next() {
                Some(wait) => tokio::time::sleep(*wait).await,
                None => return Err(e),
            },
            result => return result,
        }
    }
}

/// `request` bounded by [`K8S_REQUEST_TIMEOUT`]; running out of time is a
/// `kube::Error::Service`, like other transport failures
pub async fn bounded<T>(
    request: impl Future<Output = Result<T, kube::Error>>,
) -> Result<T, kube::Error> {
    tokio::time::timeout(K8S_REQUEST_TIMEOUT, request)
        .await
        .map_err(|elapsed| kube::Error::Service(Box::new(elapsed)))?
}

fn is_too_many_requests(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(e) if e.code == 429)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(message: &str) -> kube::Error {
        let status = k8s_openapi::serde_json::from_value(k8s_openapi::serde_json::json!({
            "status": "Failure",
            "message": message,
            "reason": "Forbidden",
            "code": 403,
        }))
        .unwrap();
        kube::Error::Api(Box::new(status))
    }

    /// The API server's message alone, not kube-rs's `Status` dump
    #[test]
    fn error_text_is_the_api_message() {
        let e = api_error("statefulsets.apps is forbidden: User \"viewer\" cannot list");
        assert!(e.to_string().contains("Status {"), "kube-rs adds the dump");
        assert_eq!(
            error_text(&e),
            "statefulsets.apps is forbidden: User \"viewer\" cannot list"
        );
        assert_eq!(error_text(&api_error("")), "Forbidden (403)");
    }
}
