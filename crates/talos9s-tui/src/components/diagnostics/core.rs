//! Core diagnostic checks that run on any Talos cluster
//!
//! These checks are CNI-agnostic and addon-agnostic.

use super::pki::{self, CertStatus, CertificateInfo};
use super::types::{DiagnosticCheck, DiagnosticContext, DiagnosticFix, FixAction};
use std::fmt::Write as _;
use talos_rs::TalosClient;
use talos9s_core::HealthIndicator;

/// Run all core system health checks
pub async fn run_system_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Memory check
    match client.memory().await {
        Ok(mem_list) => {
            if let Some(mem) = mem_list.first()
                && let Some(info) = &mem.meminfo
            {
                let usage_pct = info.usage_percent();
                let used_gb = (info.mem_total - info.mem_available) as f64 / 1_073_741_824.0;
                let total_gb = info.mem_total as f64 / 1_073_741_824.0;
                let msg = format!("{used_gb:.1} / {total_gb:.1} GB ({usage_pct:.0}%)");

                // Same thresholds as the dashboard's memory bar
                match talos9s_core::memory_health(usage_pct as f64) {
                    HealthIndicator::Error => {
                        checks.push(DiagnosticCheck::fail("memory", "Memory", &msg, None));
                    }
                    HealthIndicator::Warning => {
                        checks.push(DiagnosticCheck::warn("memory", "Memory", &msg));
                    }
                    _ => checks.push(DiagnosticCheck::pass("memory", "Memory", &msg)),
                }
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("memory", "Memory").with_details(&format!("Error: {e}")),
            );
        }
    }

    // CPU load check - threshold scales by CPU count
    match client.load_avg().await {
        Ok(load_list) => {
            if let Some(load) = load_list.first() {
                let msg = format!("{:.2} / {:.2} / {:.2}", load.load1, load.load5, load.load15);
                // Judged per core, with the dashboard's thresholds
                let cores = ctx.cpu_count;
                let per_core = |per: f64| cores.map_or(0.0, |c| c as f64 * per);
                let details = format!(
                    "Warning above {:.0} and critical above {:.0} on {} cores",
                    per_core(talos9s_core::constants::LOAD_WARN_PER_CORE),
                    per_core(talos9s_core::constants::LOAD_CRITICAL_PER_CORE),
                    cores.unwrap_or(0)
                );
                match talos9s_core::load_health(load.load1, cores) {
                    HealthIndicator::Error => checks.push(
                        DiagnosticCheck::fail("cpu_load", "CPU Load", &msg, None)
                            .with_details(&details),
                    ),
                    HealthIndicator::Warning => checks.push(
                        DiagnosticCheck::warn("cpu_load", "CPU Load", &msg).with_details(&details),
                    ),
                    // Without the core count there's nothing to judge it by
                    HealthIndicator::Unknown => checks.push(
                        DiagnosticCheck::unknown("cpu_load", "CPU Load")
                            .with_details(&format!("{msg} (core count unavailable)")),
                    ),
                    _ => checks.push(DiagnosticCheck::pass("cpu_load", "CPU Load", &msg)),
                }
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("cpu_load", "CPU Load")
                    .with_details(&format!("Error: {e}")),
            );
        }
    }

    // TODO: Add disk usage check
    // This would check ephemeral and state partition usage

    checks
}

/// Run Talos service health checks
pub async fn run_service_checks(
    client: &TalosClient,
    _ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    match client.services().await {
        Ok(services_list) => {
            // Healthy services are one line (the dashboard lists them all);
            // only problems get a row each, with their fix
            let (mut ok, mut total) = (0, 0);
            for node_services in services_list {
                for service in node_services.services {
                    total += 1;
                    let id = format!("service_{}", service.id);
                    let health = service.is_healthy();
                    let status_msg = format!(
                        "{} ({})",
                        service.state,
                        match health {
                            Some(true) => "healthy",
                            Some(false) => "unhealthy",
                            None => "no health check",
                        }
                    );
                    // No health check defined (dashboard, system extensions):
                    // running is fine, and so is a one-shot task (e.g. an
                    // extension's setup) that finished, as in the cluster view
                    let fine = health == Some(true)
                        || (health.is_none()
                            && matches!(service.state.as_str(), "Running" | "Finished"));
                    if fine {
                        ok += 1;
                    } else if health.is_none() {
                        // Flag the state without claiming it is unhealthy
                        checks.push(DiagnosticCheck::warn(&id, &service.id, &status_msg));
                    } else {
                        checks.push(DiagnosticCheck::fail(
                            &id,
                            &service.id,
                            &status_msg,
                            Some(DiagnosticFix {
                                description: format!("Restart {}", service.id),
                                action: FixAction::RestartService(service.id.clone()),
                            }),
                        ));
                    }
                }
            }
            if total > 0 {
                checks.insert(
                    0,
                    DiagnosticCheck::pass(
                        "services",
                        "Services",
                        &format!("{ok}/{total} running or completed"),
                    ),
                );
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("services", "Services")
                    .with_details(&format!("Error: {e}")),
            );
        }
    }

    checks
}

/// Upper bound for the etcd reads of one node's diagnostics
const ETCD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// This node's etcd member: whether it answers, then the etcd view's member
/// checks (apply lag, behind the leader, fragmentation, quota) for it
async fn etcd_checks(client: &TalosClient, ctx: &DiagnosticContext) -> Vec<DiagnosticCheck> {
    let target = ctx
        .node_endpoint
        .clone()
        .unwrap_or_else(|| ctx.hostname.clone());
    let read = async {
        let own = client
            .etcd_status_for_nodes(std::slice::from_ref(&target))
            .await?;
        // The member list and the leader's status only add names and the
        // behind-the-leader check; without them the rest still holds
        let members = client.etcd_members().await.unwrap_or_default();
        let leader = own
            .first()
            .filter(|s| !s.is_leader() && s.leader_id != 0)
            .and_then(|s| members.iter().find(|m| m.id == s.leader_id))
            .and_then(talos_rs::EtcdMemberInfo::ip_address);
        let leader_status = match leader {
            Some(ip) => client
                .etcd_status_for_nodes(&[ip])
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Ok::<_, talos_rs::TalosError>((own, members, leader_status))
    };
    match tokio::time::timeout(ETCD_TIMEOUT, read).await {
        Ok(Ok((own, members, leader_status))) => {
            etcd_verdict(&ctx.hostname, own.first(), &members, &leader_status)
        }
        Ok(Err(e)) => vec![
            DiagnosticCheck::fail("etcd", "Etcd", "Unreachable", None)
                .with_details(&format!("Error: {e}")),
        ],
        Err(_) => vec![
            DiagnosticCheck::unknown("etcd", "Etcd").with_details(&format!(
                "No answer from etcd within {}s",
                ETCD_TIMEOUT.as_secs()
            )),
        ],
    }
}

/// The etcd checks of the node `hostname` from its own status, the member
/// list and the leader's status (both may be empty)
fn etcd_verdict(
    hostname: &str,
    own: Option<&talos_rs::EtcdMemberStatus>,
    members: &[talos_rs::EtcdMemberInfo],
    leader_status: &[talos_rs::EtcdMemberStatus],
) -> Vec<DiagnosticCheck> {
    use crate::components::etcd::{EtcdMember, member_checks};
    let Some(own) = own else {
        return vec![DiagnosticCheck::warn("etcd", "Etcd", "No status returned")];
    };
    let info_of = |status: &talos_rs::EtcdMemberStatus, fallback: &str| {
        members
            .iter()
            .find(|m| m.id == status.member_id)
            .cloned()
            .unwrap_or_else(|| talos_rs::EtcdMemberInfo {
                id: status.member_id,
                hostname: fallback.to_string(),
                peer_urls: Vec::new(),
                client_urls: Vec::new(),
                is_learner: status.is_learner,
            })
    };
    let member = |status: &talos_rs::EtcdMemberStatus, fallback: &str| EtcdMember {
        info: info_of(status, fallback),
        status: Some(status.clone()),
        spec: None,
        running: None,
    };
    let this = member(own, hostname);
    let name = this.info.hostname.clone();
    let mut all = vec![this];
    all.extend(
        leader_status
            .iter()
            .filter(|s| s.is_leader() && s.member_id != own.member_id)
            .map(|s| member(s, "leader")),
    );
    let flagged: Vec<DiagnosticCheck> = member_checks(&all)
        .into_iter()
        .filter(|c| c.member == name)
        .map(|c| {
            if c.error {
                DiagnosticCheck::fail("etcd", "Etcd", &c.text, None)
            } else if c.defrag {
                DiagnosticCheck::warn("etcd", "Etcd", &c.text).with_fix(DiagnosticFix {
                    description: "Defragment etcd on this node".to_string(),
                    action: FixAction::EtcdDefrag,
                })
            } else {
                DiagnosticCheck::warn("etcd", "Etcd", &c.text)
            }
        })
        .collect();
    if !flagged.is_empty() {
        return flagged;
    }
    let msg = if own.is_leader() {
        "Leader, healthy".to_string()
    } else {
        match members.iter().find(|m| m.id == own.leader_id) {
            Some(leader) => format!("Follower (leader: {})", leader.hostname),
            None => format!("Follower (leader: {:x})", own.leader_id),
        }
    };
    vec![DiagnosticCheck::pass("etcd", "Etcd", &msg)]
}

/// Run core Kubernetes component checks
pub async fn run_kubernetes_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Etcd check (for control plane nodes)
    if ctx.node_role.contains("controlplane") || ctx.node_role.contains("control") {
        checks.extend(etcd_checks(client, ctx).await);
    }

    // Pod health check - use K8s API if available, otherwise skip
    // Note: CNI-specific checks are delegated to CNI providers
    if let Some(ref pod_health) = ctx.pod_health {
        if pod_health.has_issues() {
            let summary = pod_health.summary();
            let mut details = String::new();

            if !pod_health.crashing.is_empty() {
                details.push_str("Crashing pods:\n");
                for pod in &pod_health.crashing {
                    let _ = writeln!(
                        details,
                        "  {}/{} ({} restarts)",
                        pod.namespace, pod.name, pod.restart_count
                    );
                }
            }
            if !pod_health.image_pull_errors.is_empty() {
                if !details.is_empty() {
                    details.push('\n');
                }
                details.push_str("Image pull errors:\n");
                for pod in &pod_health.image_pull_errors {
                    let _ = writeln!(details, "  {}/{}", pod.namespace, pod.name);
                }
            }

            checks.push(
                DiagnosticCheck::warn("pod_health", "Pod Health", &summary)
                    .with_details(details.trim_end()),
            );
        } else {
            checks.push(DiagnosticCheck::pass(
                "pod_health",
                "Pod Health",
                &format!("{} pods on this node", pod_health.total_pods),
            ));
        }
    } else {
        // K8s API not available - show the actual error if we have it
        let details = if let Some(ref error) = ctx.k8s_error {
            format!(
                "K8s API unavailable - cannot check pod status.\n\n\
                 Error: {error}\n\n\
                 Possible causes:\n\
                 - Cluster is still starting up\n\
                 - API server not ready yet\n\
                 - kubeconfig not available from Talos\n\
                 - Network/TLS issues connecting to API server"
            )
        } else {
            "K8s API unavailable - cannot check pod status.\n\n\
             Possible causes:\n\
             - Cluster is still starting up\n\
             - API server not ready yet\n\
             - Try refreshing in a few seconds"
                .to_string()
        };
        checks.push(DiagnosticCheck::unknown("pod_health", "Pod Health").with_details(&details));
    }

    checks
}

/// Check if CNI is working (generic check via file existence)
///
/// This checks for CNI-specific config files rather than parsing logs.
/// The presence of these files indicates the CNI has initialized.
///
/// Returns (is_working, error_details)
pub async fn check_cni_health(client: &TalosClient) -> Result<bool, talos_rs::TalosError> {
    // Flannel's subnet.env, or a Cilium / Calico CNI config. `Err` when the
    // node can't be asked: that is unknown, not "no CNI".
    for path in [
        "/run/flannel/subnet.env",
        "/etc/cni/net.d/05-cilium.conflist",
        "/etc/cni/net.d/10-calico.conflist",
    ] {
        if client.file_exists(path).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Run certificate expiry checks
///
/// Checks:
/// - talosconfig client certificate (local file)
/// - talosconfig CA certificate (local file)
/// - kubeconfig client certificate (from API)
pub async fn run_certificate_checks(
    client: &TalosClient,
    ctx: &DiagnosticContext,
    config_path: Option<&str>,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Load talosconfig - use custom path if provided
    let config_result = talos_rs::TalosConfig::load(config_path);
    match config_result {
        Ok(config) => {
            // The cluster being diagnosed, not talosconfig's current one
            let context = match ctx.talos_context.as_deref() {
                Some(name) => config.get_context(name).ok(),
                None => config.current_context(),
            };
            if let Some(context) = context {
                // Check client certificate
                match context.client_cert_pem() {
                    Ok(pem_data) => match pki::parse_certificate("talosconfig", &pem_data) {
                        Ok(cert_info) => {
                            checks.push(cert_to_diagnostic_check(
                                "talosconfig_cert",
                                "talosconfig",
                                &cert_info,
                            ));
                        }
                        Err(e) => {
                            checks.push(
                                DiagnosticCheck::unknown("talosconfig_cert", "talosconfig")
                                    .with_details(&format!("Failed to parse certificate: {e}")),
                            );
                        }
                    },
                    Err(e) => {
                        checks.push(
                            DiagnosticCheck::unknown("talosconfig_cert", "talosconfig")
                                .with_details(&format!("Failed to decode certificate: {e}")),
                        );
                    }
                }

                // Check CA certificate
                match context.ca_pem() {
                    Ok(pem_data) => match pki::parse_certificate("Talos CA", &pem_data) {
                        Ok(cert_info) => {
                            checks
                                .push(cert_to_diagnostic_check("talos_ca", "Talos CA", &cert_info));
                        }
                        Err(e) => {
                            checks.push(
                                DiagnosticCheck::unknown("talos_ca", "Talos CA")
                                    .with_details(&format!("Failed to parse CA: {e}")),
                            );
                        }
                    },
                    Err(e) => {
                        checks.push(
                            DiagnosticCheck::unknown("talos_ca", "Talos CA")
                                .with_details(&format!("Failed to decode CA: {e}")),
                        );
                    }
                }
            }
        }
        Err(e) => {
            checks.push(
                DiagnosticCheck::unknown("talosconfig_cert", "talosconfig")
                    .with_details(&format!("Failed to load talosconfig: {e}")),
            );
        }
    }

    // Check kubeconfig certificate (from Talos API)
    let kubeconfig = tokio::time::timeout(pki::KUBECONFIG_TIMEOUT, client.kubeconfig())
        .await
        .map_err(|_| "timed out".to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
        .and_then(|yaml| pki::kubeconfig_certs(&yaml));
    match kubeconfig {
        Ok(certs) => {
            if let Some(cert) = &certs.client {
                checks.push(match pki::parse_base64_certificate("kubeconfig", cert) {
                    Ok(cert_info) => {
                        cert_to_diagnostic_check("kubeconfig_cert", "kubeconfig", &cert_info)
                    }
                    Err(e) => DiagnosticCheck::unknown("kubeconfig_cert", "kubeconfig")
                        .with_details(&format!("Failed to parse kubeconfig cert: {e}")),
                });
            }
        }
        Err(e) => {
            // kubeconfig may not be available yet during cluster bootstrap
            checks.push(
                DiagnosticCheck::unknown("kubeconfig_cert", "kubeconfig")
                    .with_details(&format!("kubeconfig not available: {e}")),
            );
        }
    }

    checks
}

/// Convert a CertificateInfo to a DiagnosticCheck
fn cert_to_diagnostic_check(id: &str, name: &str, cert: &CertificateInfo) -> DiagnosticCheck {
    let message = cert.expiry_text();

    let details = format!(
        "Subject: {}\nIssuer: {}\nExpires: {}\nDays remaining: {}",
        cert.subject,
        cert.issuer,
        cert.not_after.format("%Y-%m-%d %H:%M:%S UTC"),
        cert.days_remaining
    );

    let renewal_hint = match name {
        "talosconfig" => {
            Some("To renew, run:\n  talosctl config new --roles=os:admin new-admin.yaml")
        }
        "kubeconfig" => Some(
            "Kubeconfig certificates are managed by Talos.\nRegenerate with: talosctl kubeconfig",
        ),
        _ => None,
    };

    let full_details = if let Some(hint) = renewal_hint {
        format!("{details}\n\n{hint}")
    } else {
        details
    };

    match cert.status {
        CertStatus::Valid => DiagnosticCheck::pass(id, name, &message).with_details(&full_details),
        CertStatus::Warning => {
            let mut check = DiagnosticCheck::warn(id, name, &message).with_details(&full_details);

            // Add fix suggestion for talosconfig
            if name == "talosconfig" {
                check = check.with_fix(DiagnosticFix {
                    description: "Renew talosconfig certificate".to_string(),
                    action: FixAction::HostCommand {
                        command: "talosctl config new --roles=os:admin new-admin.yaml".to_string(),
                        description: "Generate new talosconfig".to_string(),
                    },
                });
            }
            check
        }
        CertStatus::Critical | CertStatus::Expired => {
            let mut check =
                DiagnosticCheck::fail(id, name, &message, None).with_details(&full_details);

            // Add fix suggestion for talosconfig
            if name == "talosconfig" {
                check = check.with_fix(DiagnosticFix {
                    description: "Renew talosconfig certificate (URGENT)".to_string(),
                    action: FixAction::HostCommand {
                        command: "talosctl config new --roles=os:admin new-admin.yaml".to_string(),
                        description: "Generate new talosconfig".to_string(),
                    },
                });
            }
            check
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::CheckStatus;
    use super::*;
    use talos_rs::{EtcdMemberInfo, EtcdMemberStatus};

    const MIB: i64 = 1 << 20;

    fn info(id: u64) -> EtcdMemberInfo {
        EtcdMemberInfo {
            id,
            hostname: format!("lab{id}"),
            peer_urls: vec![format!("https://10.0.0.{id}:2380")],
            client_urls: Vec::new(),
            is_learner: false,
        }
    }

    fn status(id: u64, leader: u64, db: i64, in_use: i64, raft: (u64, u64)) -> EtcdMemberStatus {
        EtcdMemberStatus {
            node: format!("lab{id}"),
            member_id: id,
            protocol_version: "3.5".to_string(),
            db_size: db,
            db_size_in_use: in_use,
            leader_id: leader,
            raft_index: raft.0,
            raft_term: 7,
            raft_applied_index: raft.1,
            errors: Vec::new(),
            is_learner: false,
        }
    }

    fn messages(checks: &[DiagnosticCheck]) -> Vec<(CheckStatus, String)> {
        checks
            .iter()
            .map(|c| (c.status.clone(), c.message.clone()))
            .collect()
    }

    #[test]
    fn healthy_member_passes_with_the_leader_named() {
        let members = [info(1), info(2)];
        let leader = status(1, 1, 100 * MIB, 90 * MIB, (5_000, 5_000));
        let own = status(2, 1, 100 * MIB, 90 * MIB, (4_990, 4_990));
        assert_eq!(
            messages(&etcd_verdict(
                "lab2",
                Some(&own),
                &members,
                std::slice::from_ref(&leader)
            )),
            [(CheckStatus::Pass, "Follower (leader: lab1)".to_string())]
        );
        assert_eq!(
            messages(&etcd_verdict("lab1", Some(&leader), &members, &[])),
            [(CheckStatus::Pass, "Leader, healthy".to_string())]
        );
        // No status for this member: a warning, not a failure
        assert_eq!(
            messages(&etcd_verdict("lab1", None, &members, &[]))[0].0,
            CheckStatus::Warn
        );
    }

    /// The etcd view's member checks, for this node only
    #[test]
    fn flags_lag_behind_leader_and_defrag_of_this_member() {
        let members = [info(1), info(2)];
        // The leader is fragmented too, but that's its own node's check
        let leader = status(1, 1, 900 * MIB, 300 * MIB, (100_000, 100_000));
        let own = status(2, 1, 900 * MIB, 300 * MIB, (85_000, 78_000));
        let checks = etcd_verdict("lab2", Some(&own), &members, &[leader]);
        let found = messages(&checks);
        assert_eq!(
            found[0],
            (
                CheckStatus::Fail,
                "7000 entries committed but not applied (slow disk?)".to_string()
            )
        );
        assert!(found.contains(&(
            CheckStatus::Warn,
            "15000 entries behind the leader".to_string()
        )));
        let defrag: Vec<&DiagnosticCheck> = checks.iter().filter(|c| c.fix.is_some()).collect();
        assert_eq!(defrag.len(), 1, "{found:?}");
        assert!(defrag[0].message.starts_with("defrag would reclaim 600"));
        assert_eq!(
            defrag[0]
                .fix
                .as_ref()
                .unwrap()
                .action
                .command(Some("lab"), "10.0.0.2:50000"),
            "talosctl --context lab -n 10.0.0.2 etcd defrag"
        );

        // Without the leader's status there is nothing to compare against
        let alone = messages(&etcd_verdict("lab2", Some(&own), &members, &[]));
        assert!(!alone.iter().any(|(_, m)| m.contains("behind")));
    }
}
