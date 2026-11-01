//! CNI detection and diagnostics
//!
//! This module handles:
//! 1. Auto-detecting which CNI is installed (Flannel, Cilium, Calico)
//! 2. Running CNI-specific diagnostic checks
//! 3. Providing CNI-specific fixes

mod cilium;
mod flannel;

pub use cilium::detect_ebpf_mode;
pub use flannel::run_flannel_checks;

use super::k8s;
use super::types::{CniInfo, CniPodInfo, CniType, DiagnosticCheck, DiagnosticContext};
use kube::Client;
use talos_rs::TalosClient;

/// Detect which CNI is installed in the cluster using a pre-created K8s client
///
/// Uses K8s API if client provided, falls back to file-based detection.
/// Returns both the CNI type and detailed pod info if K8s API is available.
pub async fn detect_cni_with_client(
    talos_client: &TalosClient,
    k8s_client: Option<&Client>,
) -> (CniType, Option<CniInfo>) {
    // Try K8s API detection first if client available
    if let Some(client) = k8s_client {
        match k8s::detect_cni_from_k8s(client).await {
            Ok(info) => {
                if info.cni_type != CniType::Unknown {
                    tracing::info!(
                        "CNI detected via K8s API: {:?} ({} pods)",
                        info.cni_type,
                        info.pods.len()
                    );
                    // Convert k8s::CniInfo to types::CniInfo
                    let cni_info = CniInfo {
                        cni_type: info.cni_type.clone(),
                        pods: info
                            .pods
                            .iter()
                            .map(|p| CniPodInfo {
                                name: p.name.clone(),
                                node_name: p.node_name.clone(),
                                phase: p.phase.clone(),
                                ready: p.ready,
                                restart_count: p.restart_count,
                            })
                            .collect(),
                    };
                    return (info.cni_type, Some(cni_info));
                }
            }
            Err(e) => {
                tracing::warn!("K8s CNI detection failed: {}", e);
            }
        }
    }

    // Fall back to file-based detection (more reliable than log parsing)
    tracing::info!("Falling back to file-based CNI detection");
    let cni_type = detect_cni_from_files(talos_client).await;
    (cni_type, None)
}

/// Detect CNI from config files (fallback method)
///
/// Checks for CNI-specific files rather than parsing logs.
/// This is more reliable because files represent actual state.
async fn detect_cni_from_files(client: &TalosClient) -> CniType {
    // Check for Flannel subnet.env (definitive indicator)
    if client.read_file("/run/flannel/subnet.env").await.is_ok() {
        return CniType::Flannel;
    }

    // Check for Cilium CNI config
    if client
        .read_file("/etc/cni/net.d/05-cilium.conflist")
        .await
        .is_ok()
    {
        return CniType::Cilium;
    }

    // Check for Calico CNI config
    if client
        .read_file("/etc/cni/net.d/10-calico.conflist")
        .await
        .is_ok()
    {
        return CniType::Calico;
    }

    CniType::Unknown
}

/// Run CNI-specific diagnostic checks based on detected CNI type
pub async fn run_cni_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
    k8s_client: Option<&Client>,
) -> Vec<DiagnosticCheck> {
    match ctx.cni_type {
        CniType::Flannel => flannel::run_flannel_checks(client, ctx).await,
        CniType::Cilium => cilium::run_cilium_checks(ctx, k8s_client).await,
        CniType::Calico => run_calico_checks(client, ctx).await,
        CniType::Unknown | CniType::None => run_generic_cni_checks(client, ctx).await,
    }
}

/// Generic CNI checks when we don't know the CNI type
async fn run_generic_cni_checks(
    client: &TalosClient,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Just check if CNI is working at a basic level
    let (cni_ok, error) = super::core::check_cni_health(client).await;

    if cni_ok {
        checks.push(DiagnosticCheck::pass("cni", "CNI", "OK"));
    } else {
        checks.push(
            DiagnosticCheck::fail("cni", "CNI", "Network setup failed", None)
                .with_details(&error.unwrap_or_else(|| "Unknown error".to_string())),
        );
    }

    checks
}

/// Calico-specific checks
async fn run_calico_checks(_client: &TalosClient, ctx: &DiagnosticContext) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check Calico pod health if we have K8s API info
    if let Some(ref cni_info) = ctx.cni_info {
        checks.push(check_cni_pods("Calico Pods", cni_info));
    }

    // TODO: Check br_netfilter (depends on Calico datapath mode)
    // - iptables mode: requires br_netfilter
    // - eBPF mode: does not require br_netfilter

    checks.push(DiagnosticCheck::pass(
        "cni",
        "CNI (Calico)",
        if ctx.cni_info.is_some() {
            "OK"
        } else {
            "Detected"
        },
    ));

    checks
}

/// Generic helper to check CNI pod health
fn check_cni_pods(name: &str, cni_info: &CniInfo) -> DiagnosticCheck {
    if cni_info.pods.is_empty() {
        return DiagnosticCheck::warn("cni_pods", name, "No pods found")
            .with_details("Could not find CNI pods in kube-system namespace.");
    }

    let healthy = cni_info.are_pods_healthy();
    let summary = cni_info.pod_health_summary();

    if healthy {
        DiagnosticCheck::pass("cni_pods", name, &summary)
    } else {
        // Find unhealthy pods
        let unhealthy: Vec<_> = cni_info
            .pods
            .iter()
            .filter(|p| p.phase != "Running" || !p.ready)
            .collect();

        let details = unhealthy
            .iter()
            .map(|p| format!("  {} - {} (ready: {})", p.name, p.phase, p.ready))
            .collect::<Vec<_>>()
            .join("\n");

        DiagnosticCheck::fail("cni_pods", name, &summary, None)
            .with_details(&format!("Unhealthy pods:\n{}", details))
    }
}
