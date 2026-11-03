//! cert-manager addon diagnostics
//!
//! Checks for cert-manager health:
//! - Pod health (controller, cainjector, webhook)
//! - Webhook readiness (needed to issue certificates)

use crate::components::diagnostics::types::DiagnosticCheck;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Client,
    api::{Api, ListParams},
};

/// `app.kubernetes.io/name` of cert-manager's pods, in the upstream
/// manifests and the Helm chart
const POD_NAMES: [&str; 3] = ["cert-manager", "cainjector", "webhook"];

/// Run cert-manager diagnostic checks
pub async fn run_checks(client: &Client) -> Vec<DiagnosticCheck> {
    // By label in every namespace: it isn't always installed in
    // `cert-manager`
    let pods: Api<Pod> = Api::all(client.clone());
    let selector = format!("app.kubernetes.io/name in ({})", POD_NAMES.join(","));
    match pods.list(&ListParams::default().labels(&selector)).await {
        Ok(list) => assess(&list.items),
        Err(e) => vec![
            DiagnosticCheck::unknown("cert_manager_pods", "cert-manager Pods").with_details(
                &format!("Failed to list pods: {}", crate::kube_list::error_text(&e)),
            ),
            DiagnosticCheck::unknown("cert_manager_webhook", "cert-manager Webhook").with_details(
                &format!("Failed to list pods: {}", crate::kube_list::error_text(&e)),
            ),
        ],
    }
}

fn label<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
    pod.metadata.labels.as_ref()?.get(key).map(String::as_str)
}

fn is_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

/// Pod and webhook checks from the pods matching [`POD_NAMES`]. Only the
/// namespaces holding the controller count ("webhook" alone is a common
/// name), and finished pods (the startupapicheck Job) are left out.
fn assess(pods: &[Pod]) -> Vec<DiagnosticCheck> {
    let controller_namespaces: Vec<&str> = pods
        .iter()
        .filter(|p| label(p, "app.kubernetes.io/name") == Some("cert-manager"))
        .filter_map(|p| p.metadata.namespace.as_deref())
        .collect();
    let pods: Vec<&Pod> = pods
        .iter()
        .filter(|p| {
            p.metadata
                .namespace
                .as_deref()
                .is_some_and(|ns| controller_namespaces.contains(&ns))
        })
        .filter(|p| {
            let phase = p.status.as_ref().and_then(|s| s.phase.as_deref());
            !matches!(phase, Some("Succeeded"))
        })
        .collect();

    if pods.is_empty() {
        let none = |id, name| {
            DiagnosticCheck::fail(id, name, "No pods found", None).with_details(
                "cert-manager's CRDs are installed but no cert-manager controller pod runs.",
            )
        };
        return vec![
            none("cert_manager_pods", "cert-manager Pods"),
            DiagnosticCheck::unknown("cert_manager_webhook", "cert-manager Webhook"),
        ];
    }

    let total = pods.len();
    let unhealthy: Vec<String> = pods
        .iter()
        .filter(|p| {
            let phase = p.status.as_ref().and_then(|s| s.phase.as_deref());
            !(phase == Some("Running") && is_ready(p))
        })
        .map(|p| {
            let phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("Unknown");
            format!(
                "{}/{}: {} (ready: {})",
                p.metadata.namespace.as_deref().unwrap_or_default(),
                p.metadata.name.as_deref().unwrap_or_default(),
                phase,
                is_ready(p)
            )
        })
        .collect();
    let summary = format!("{}/{} healthy", total - unhealthy.len(), total);
    let pods_check = if unhealthy.is_empty() {
        DiagnosticCheck::pass("cert_manager_pods", "cert-manager Pods", &summary)
    } else {
        DiagnosticCheck::warn("cert_manager_pods", "cert-manager Pods", &summary)
            .with_details(&format!("Unhealthy pods:\n{}", unhealthy.join("\n")))
    };

    let webhooks: Vec<&&Pod> = pods
        .iter()
        .filter(|p| label(p, "app.kubernetes.io/name") == Some("webhook"))
        .collect();
    let webhook_check = if webhooks.is_empty() {
        DiagnosticCheck::warn("cert_manager_webhook", "cert-manager Webhook", "Not found")
            .with_details("Webhook pod not found. Certificate validation may not work.")
    } else if webhooks.iter().any(|p| is_ready(p)) {
        DiagnosticCheck::pass("cert_manager_webhook", "cert-manager Webhook", "Ready")
    } else {
        DiagnosticCheck::warn("cert_manager_webhook", "cert-manager Webhook", "Not ready")
            .with_details(
                "Webhook pod exists but is not ready. New certificates may fail to be issued.",
            )
    };

    vec![pods_check, webhook_check]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::diagnostics::types::CheckStatus;
    use k8s_openapi::serde_json::json;

    fn pod(ns: &str, name: &str, app: &str, phase: &str, ready: bool) -> Pod {
        k8s_openapi::serde_json::from_value(json!({
            "metadata": {"name": name, "namespace": ns,
                         "labels": {"app.kubernetes.io/name": app}},
            "status": {"phase": phase, "conditions": [
                {"type": "Ready", "status": if ready { "True" } else { "False" }}
            ]},
        }))
        .unwrap()
    }

    #[test]
    fn found_outside_the_cert_manager_namespace() {
        let pods = [
            pod("security", "cm", "cert-manager", "Running", true),
            pod("security", "cm-cainjector", "cainjector", "Running", true),
            pod("security", "cm-webhook", "webhook", "Running", true),
            // The startupapicheck Job's pod has finished
            pod(
                "security",
                "cm-startupapicheck",
                "startupapicheck",
                "Succeeded",
                false,
            ),
            // Another project's webhook, not cert-manager's
            pod("other", "webhook", "webhook", "Pending", false),
        ];
        let checks = assess(&pods);
        assert_eq!(checks[0].status, CheckStatus::Pass, "{:?}", checks[0]);
        assert_eq!(checks[0].message, "3/3 healthy");
        assert_eq!(checks[1].status, CheckStatus::Pass);
    }

    #[test]
    fn unready_webhook_warns() {
        let pods = [
            pod("cert-manager", "cm", "cert-manager", "Running", true),
            pod("cert-manager", "cm-webhook", "webhook", "Running", false),
        ];
        let checks = assess(&pods);
        assert_eq!(checks[0].status, CheckStatus::Warn);
        assert_eq!(checks[1].message, "Not ready");
    }
}
