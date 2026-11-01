//! Addon detection and diagnostics
//!
//! This module handles detecting and checking common Kubernetes addons:
//! - cert-manager
//! - external-secrets
//! - And more...
//!
//! Addons are detected by their CRDs (one API call).

mod cert_manager;

use super::types::{DiagnosticCheck, DiagnosticContext};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    Client,
    api::{Api, ListParams},
};
use talos9s_core::constants::{
    ARGOCD_CRDS, CERT_MANAGER_CRDS, EXTERNAL_SECRETS_CRDS, FLUX_CRDS, KYVERNO_CRDS,
};

/// Addons found by their CRDs: (name, any of these CRDs)
const ADDONS: &[(&str, &[&str])] = &[
    ("cert-manager", CERT_MANAGER_CRDS),
    ("external-secrets", EXTERNAL_SECRETS_CRDS),
    ("kyverno", KYVERNO_CRDS),
    ("argocd", ARGOCD_CRDS),
    ("flux", FLUX_CRDS),
];

/// Addons detected in the cluster, by name
#[derive(Debug, Clone, Default)]
pub struct DetectedAddons {
    pub names: Vec<&'static str>,
}

impl DetectedAddons {
    /// Check if any addons are detected
    pub fn any_detected(&self) -> bool {
        !self.names.is_empty()
    }

    pub fn has(&self, name: &str) -> bool {
        self.names.contains(&name)
    }

    /// The addons whose CRDs are among `crd_names`
    fn from_crds(crd_names: &[String]) -> Self {
        let names = ADDONS
            .iter()
            .filter(|(_, crds)| crds.iter().any(|crd| crd_names.iter().any(|n| n == crd)))
            .map(|(name, _)| *name)
            .collect();
        DetectedAddons { names }
    }
}

/// Detect which addons are installed by their CRDs: one API call, names
/// only (full CRDs carry their schemas, megabytes on a busy cluster)
pub async fn detect_addons(client: &Client) -> DetectedAddons {
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let addons = match crds.list_metadata(&ListParams::default()).await {
        Ok(list) => {
            let names: Vec<String> = list
                .items
                .into_iter()
                .filter_map(|crd| crd.metadata.name)
                .collect();
            DetectedAddons::from_crds(&names)
        }
        Err(e) => {
            tracing::warn!("Failed to list CRDs for addon detection: {}", e);
            DetectedAddons::default()
        }
    };
    tracing::info!("Detected addons: {:?}", addons.names);
    addons
}

/// Run addon-specific diagnostic checks
pub async fn run_addon_checks(
    client: Option<&Client>,
    addons: &DetectedAddons,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    let Some(k8s_client) = client else {
        // No K8s client - can't check addon health
        return checks;
    };

    // cert-manager checks
    if addons.has("cert-manager") {
        checks.extend(cert_manager::run_checks(k8s_client).await);
    }

    // TODO: Add checks for other addons as needed
    // if addons.external_secrets {
    //     checks.extend(external_secrets::run_checks(k8s_client).await);
    // }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addons_from_crds() {
        let crds = vec![
            CERT_MANAGER_CRDS[0].to_string(),
            "widgets.example.com".to_string(),
        ];
        let addons = DetectedAddons::from_crds(&crds);
        assert_eq!(addons.names, ["cert-manager"]);
        assert!(addons.has("cert-manager"));
        assert!(!DetectedAddons::from_crds(&[]).any_detected());
    }
}
