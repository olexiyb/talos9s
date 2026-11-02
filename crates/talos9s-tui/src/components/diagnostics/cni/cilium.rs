//! Cilium CNI diagnostics
//!
//! Cilium-specific checks:
//! - Per-node Cilium agent status (DaemonSet pods)
//! - Cilium Operator deployment status
//! - Hubble Relay deployment status (if enabled)
//! - Cilium eBPF mode detection (kube-proxy replacement)
//! - Cilium + KubeSpan: native routing set up the way Talos documents
//!
//! Philosophy: Check actual K8s API state, not logs. The pod status
//! and readiness conditions are the definitive indicators of health.

use crate::components::diagnostics::k8s::DeploymentReplicas;
use crate::components::diagnostics::types::{
    CniInfo, CniPodInfo, DiagnosticCheck, DiagnosticContext,
};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{Client, api::Api};
use talos_rs::{KubeSpanSettings, get_kubespan_settings_for_node};

/// Run Cilium-specific diagnostic checks
pub async fn run_cilium_checks(
    ctx: &DiagnosticContext,
    k8s_client: Option<&Client>,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check per-node Cilium agent health if we have K8s API info
    if let Some(ref cni_info) = ctx.cni_info {
        checks.push(check_cilium_agents_per_node(cni_info));
    }

    // Check Cilium Operator and Hubble if we have K8s client
    if let Some(client) = k8s_client {
        // Check Cilium Operator
        checks.push(check_cilium_operator(client).await);

        // Check Hubble Relay (only if it exists)
        if let Some(hubble_check) = check_hubble_relay(client).await {
            checks.push(hubble_check);
        }

        // Check Cilium eBPF + KubeSpan compatibility
        if let Some(compat_check) = check_kubespan_compatibility(ctx, client).await {
            checks.push(compat_check);
        }
    }

    // Add overall CNI status check
    let overall_status = determine_overall_status(&checks, ctx);
    checks.push(overall_status);

    checks
}

/// Check per-node Cilium agent status
///
/// Shows the health of Cilium agent pods on each node.
/// This is critical for understanding which nodes have networking issues.
fn check_cilium_agents_per_node(cni_info: &CniInfo) -> DiagnosticCheck {
    // Filter to only cilium-agent pods (not operator, hubble, etc.)
    let agent_pods: Vec<&CniPodInfo> = cni_info
        .pods
        .iter()
        .filter(|p| {
            let name = p.name.to_lowercase();
            // Match cilium agent pods, exclude operator/hubble
            name.starts_with("cilium-")
                && !name.contains("operator")
                && !name.contains("hubble")
                && !name.contains("envoy")
        })
        .collect();

    if agent_pods.is_empty() {
        return DiagnosticCheck::warn("cilium_agents", "Cilium Agents", "No agent pods found")
            .with_details(
                "Could not find Cilium agent pods in kube-system namespace.\n\
                       Expected pods matching 'cilium-*' (DaemonSet).",
            );
    }

    let total = agent_pods.len();
    let healthy: Vec<_> = agent_pods
        .iter()
        .filter(|p| p.phase == "Running" && p.ready)
        .collect();
    let unhealthy: Vec<_> = agent_pods
        .iter()
        .filter(|p| p.phase != "Running" || !p.ready)
        .collect();

    let total_restarts: i32 = agent_pods.iter().map(|p| p.restart_count).sum();

    if unhealthy.is_empty() {
        // All healthy
        let message = if total_restarts > 0 {
            format!(
                "{}/{} agents ready ({} restarts)",
                healthy.len(),
                total,
                total_restarts
            )
        } else {
            format!("{}/{} agents ready", healthy.len(), total)
        };

        let details = format_agent_details(&agent_pods);
        DiagnosticCheck::pass("cilium_agents", "Cilium Agents", &message).with_details(&details)
    } else {
        // Some unhealthy
        let message = format!("{}/{} agents ready", healthy.len(), total);
        let details = format_agent_details_with_issues(&agent_pods, &unhealthy);

        DiagnosticCheck::fail("cilium_agents", "Cilium Agents", &message, None)
            .with_details(&details)
    }
}

/// Format per-node agent details for healthy clusters
fn format_agent_details(pods: &[&CniPodInfo]) -> String {
    let mut lines = vec!["Per-node status:".to_string()];

    for pod in pods {
        let node = pod.node_name.as_deref().unwrap_or("unknown");
        let status = if pod.phase == "Running" && pod.ready {
            "Ready"
        } else {
            &pod.phase
        };
        let restarts = if pod.restart_count > 0 {
            format!(" ({} restarts)", pod.restart_count)
        } else {
            String::new()
        };
        lines.push(format!("  {status} on {node}{restarts}"));
    }

    lines.join("\n")
}

/// Format per-node agent details highlighting issues
fn format_agent_details_with_issues(
    all_pods: &[&CniPodInfo],
    unhealthy: &[&&CniPodInfo],
) -> String {
    let mut lines = vec!["Per-node status:".to_string()];

    // Show unhealthy first
    lines.push("Unhealthy:".to_string());
    for pod in unhealthy {
        let node = pod.node_name.as_deref().unwrap_or("unknown");
        let status = format!("{} (ready: {})", pod.phase, pod.ready);
        lines.push(format!("  {} on {} - {}", pod.name, node, status));
    }

    // Then healthy
    let healthy_count = all_pods.len() - unhealthy.len();
    if healthy_count > 0 {
        lines.push(format!("\nHealthy: {healthy_count} nodes"));
    }

    lines.push("\nTip: Check agent logs with:".to_string());
    lines.push("  kubectl logs -n kube-system -l k8s-app=cilium".to_string());

    lines.join("\n")
}

/// Check Cilium Operator deployment status
///
/// Note: Cilium operator defaults to 2 replicas for HA, but on single-node
/// clusters only 1 can run (due to pod anti-affinity). We consider this OK
/// if at least 1 replica is available and functional.
async fn check_cilium_operator(client: &Client) -> DiagnosticCheck {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");

    // Try cilium-operator first, then cilium-operator-generic
    let operator_names = ["cilium-operator", "cilium-operator-generic"];

    for name in operator_names {
        match deployments.get(name).await {
            Ok(deployment) => {
                // Unavailable replicas show single-node scheduling limits
                let DeploymentReplicas {
                    desired: replicas,
                    ready,
                    available,
                    unavailable,
                } = DeploymentReplicas::of(&deployment);

                if replicas == 0 {
                    return DiagnosticCheck::pass(
                        "cilium_operator",
                        "Cilium Operator",
                        "scaled to 0",
                    );
                }
                if ready >= replicas {
                    // All replicas ready - perfect
                    return DiagnosticCheck::pass(
                        "cilium_operator",
                        "Cilium Operator",
                        &format!("{ready}/{replicas} ready"),
                    );
                } else if available >= 1 && unavailable > 0 {
                    // At least one replica is available and functional.
                    // The unavailable ones are likely pending due to pod anti-affinity
                    // on a cluster with fewer nodes than desired replicas - this is OK.
                    return DiagnosticCheck::pass(
                        "cilium_operator",
                        "Cilium Operator",
                        &format!("{ready}/{replicas} ready (HA limited)"),
                    ).with_details(
                        &format!("{unavailable} replica(s) pending - likely due to pod anti-affinity.\n\
                                 This is normal on clusters with fewer nodes than operator replicas.\n\
                                 The operator is functional with {available} available replica(s).")
                    );
                } else if available > 0 {
                    // Available but something else going on
                    return DiagnosticCheck::warn(
                        "cilium_operator",
                        "Cilium Operator",
                        &format!("{ready}/{replicas} ready ({available} available)"),
                    );
                }
                return DiagnosticCheck::fail(
                    "cilium_operator",
                    "Cilium Operator",
                    &format!("{ready}/{replicas} ready"),
                    None,
                )
                .with_details(
                    "Cilium Operator is not ready. This can affect:\n\
                              - IP address allocation\n\
                              - CiliumNetworkPolicy enforcement\n\
                              - Cluster mesh operations\n\n\
                              Check operator logs:\n\
                              kubectl logs -n kube-system -l name=cilium-operator",
                );
            }
            Err(_) => continue,
        }
    }

    // No operator found - this might be fine for some Cilium configurations
    DiagnosticCheck::unknown("cilium_operator", "Cilium Operator").with_details(
        "Cilium Operator deployment not found in kube-system.\n\
                       Some Cilium features may require the operator.",
    )
}

/// Check Hubble Relay deployment status (returns None if Hubble not deployed)
async fn check_hubble_relay(client: &Client) -> Option<DiagnosticCheck> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), "kube-system");

    match deployments.get("hubble-relay").await {
        Ok(deployment) => {
            let DeploymentReplicas {
                desired: replicas,
                ready,
                ..
            } = DeploymentReplicas::of(&deployment);

            if replicas == 0 {
                Some(DiagnosticCheck::pass(
                    "hubble_relay",
                    "Hubble Relay",
                    "scaled to 0",
                ))
            } else if ready >= replicas {
                Some(DiagnosticCheck::pass(
                    "hubble_relay",
                    "Hubble Relay",
                    &format!("{ready}/{replicas} ready"),
                ))
            } else {
                Some(
                    DiagnosticCheck::fail(
                        "hubble_relay",
                        "Hubble Relay",
                        &format!("{ready}/{replicas} ready"),
                        None,
                    )
                    .with_details(
                        "Hubble Relay is not ready. Network flow observability unavailable.\n\n\
                              Check relay logs:\n\
                              kubectl logs -n kube-system -l k8s-app=hubble-relay",
                    ),
                )
            }
        }
        Err(_) => None, // Hubble not deployed - not an error
    }
}

/// Talos' guide to combining Cilium and KubeSpan
const CILIUM_KUBESPAN_DOCS: &str = "https://docs.siderolabs.com/talos/v1.14/networking/kubespan";

/// Cilium + KubeSpan, from Cilium's settings (`kube-system/cilium-config`)
/// and the node's KubeSpanConfig. `None` without KubeSpan, or when either
/// can't be read: unknown is not a problem.
async fn check_kubespan_compatibility(
    ctx: &DiagnosticContext,
    k8s_client: &Client,
) -> Option<DiagnosticCheck> {
    let endpoint = ctx.node_endpoint.as_ref()?;
    let kubespan = get_kubespan_settings_for_node(
        ctx.talos_context.as_deref(),
        endpoint,
        ctx.config_path.as_deref(),
    )
    .await
    .ok()?;
    if !kubespan.enabled {
        return None;
    }
    let config_maps: Api<ConfigMap> = Api::namespaced(k8s_client.clone(), "kube-system");
    let data = config_maps.get("cilium-config").await.ok()?.data?;
    let setting = |key: &str| data.get(key).map(String::as_str);
    Some(kubespan_compatibility(
        setting("routing-mode"),
        setting("enable-host-legacy-routing"),
        kubespan,
    ))
}

/// Talos 1.13+ documents two working setups: Cilium's default tunnel
/// routing as is, and native routing with legacy host routing (eBPF host
/// routing bypasses the kernel routing table KubeSpan uses) plus KubeSpan
/// advertising the pod networks
fn kubespan_compatibility(
    routing_mode: Option<&str>,
    host_legacy_routing: Option<&str>,
    kubespan: KubeSpanSettings,
) -> DiagnosticCheck {
    const ID: &str = "cilium_kubespan_compat";
    const NAME: &str = "Cilium + KubeSpan";
    if routing_mode != Some("native") {
        return DiagnosticCheck::pass(ID, NAME, "tunnel routing, works with KubeSpan as is");
    }
    let mut problems = Vec::new();
    if host_legacy_routing != Some("true") {
        problems.push(
            "eBPF host routing is on (set bpf.hostLegacyRouting: true): it bypasses the \
             kernel routes KubeSpan uses",
        );
    }
    if !kubespan.advertise_kubernetes_networks {
        problems
            .push("KubeSpan doesn't route pod networks (set advertiseKubernetesNetworks: true)");
    }
    if problems.is_empty() {
        DiagnosticCheck::pass(
            ID,
            NAME,
            "native routing with legacy host routing and pod networks over KubeSpan",
        )
    } else {
        DiagnosticCheck::warn(
            ID,
            NAME,
            &format!("native routing: {}", problems.join("; ")),
        )
        .with_details(&format!(
            "Cilium routes pods natively, which Talos supports with KubeSpan only when \
             eBPF host routing is off and KubeSpan advertises the Kubernetes networks. \
             Otherwise traffic between nodes can take different paths each way and \
             connections break.\n\nSee: {CILIUM_KUBESPAN_DOCS}"
        ))
    }
}

/// Determine overall Cilium CNI status based on component checks
fn determine_overall_status(
    checks: &[DiagnosticCheck],
    ctx: &DiagnosticContext,
) -> DiagnosticCheck {
    use crate::components::diagnostics::types::CheckStatus;

    // Check if any critical checks failed
    let has_failure = checks.iter().any(|c| c.status == CheckStatus::Fail);
    let has_warning = checks.iter().any(|c| c.status == CheckStatus::Warn);

    // Name the checks behind the summary, so it says something on its own
    let named = |status: CheckStatus| {
        checks
            .iter()
            .filter(|c| c.status == status)
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if has_failure {
        DiagnosticCheck::fail(
            "cni",
            "CNI (Cilium)",
            &format!("unhealthy: {}", named(CheckStatus::Fail)),
            None,
        )
    } else if has_warning {
        DiagnosticCheck::warn(
            "cni",
            "CNI (Cilium)",
            &format!("warnings in {}", named(CheckStatus::Warn)),
        )
    } else if ctx.cni_info.is_some() {
        DiagnosticCheck::pass("cni", "CNI (Cilium)", "OK")
    } else {
        DiagnosticCheck::pass("cni", "CNI (Cilium)", "Detected")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::diagnostics::types::CheckStatus;

    fn kubespan(advertise: bool) -> KubeSpanSettings {
        KubeSpanSettings {
            enabled: true,
            advertise_kubernetes_networks: advertise,
        }
    }

    /// The two setups Talos documents pass: kprod (tunnel) and bc (native
    /// routing with legacy host routing and pod networks over KubeSpan)
    #[test]
    fn documented_setups_pass() {
        let tunnel = kubespan_compatibility(Some("tunnel"), Some("true"), kubespan(false));
        assert_eq!(tunnel.status, CheckStatus::Pass);
        let native = kubespan_compatibility(Some("native"), Some("true"), kubespan(true));
        assert_eq!(native.status, CheckStatus::Pass);
        // Cilium's default when the key is missing is tunnel routing
        let default = kubespan_compatibility(None, None, kubespan(false));
        assert_eq!(default.status, CheckStatus::Pass);
    }

    /// Native routing names what's missing, so the warning stands on its own
    #[test]
    fn native_routing_names_what_is_missing() {
        let check = kubespan_compatibility(Some("native"), Some("false"), kubespan(false));
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.message.contains("bpf.hostLegacyRouting: true"));
        assert!(check.message.contains("advertiseKubernetesNetworks: true"));

        let check = kubespan_compatibility(Some("native"), Some("true"), kubespan(false));
        assert!(!check.message.contains("hostLegacyRouting"));
        assert!(check.message.contains("advertiseKubernetesNetworks"));
    }

    #[test]
    fn summary_names_the_checks_behind_it() {
        let checks = vec![
            DiagnosticCheck::pass("cilium_agents", "Cilium agents", "4/4"),
            DiagnosticCheck::warn("hubble_relay", "Hubble Relay", "0/1 ready"),
        ];
        let summary = determine_overall_status(&checks, &DiagnosticContext::default());
        assert_eq!(summary.message, "warnings in Hubble Relay");
    }
}
