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
