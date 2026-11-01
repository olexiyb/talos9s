//! etcd quorum assessment: how healthy the cluster is now and whether taking
//! one node down (reboot, shutdown, upgrade) keeps quorum.
//!
//! Learners don't vote, so they count neither toward the quorum size nor
//! toward the healthy voters. A member counts as healthy when it answered the
//! status call. When the status call failed or answered for no one, health is
//! unknown, never assumed.

use crate::{QuorumState, SafetyStatus};
use talos_rs::{EtcdMemberInfo, EtcdMemberStatus};

/// Quorum state of the etcd cluster as seen by one members + status fetch
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcdQuorum {
    /// Voting members (learners excluded)
    pub voters: usize,
    /// Voters that answered the status call; `None` when unknown
    pub healthy: Option<usize>,
    /// Voters needed for quorum
    pub quorum_needed: usize,
    /// IDs of the voters that answered
    healthy_ids: Vec<u64>,
    /// ID of the leader, if a status named one
    leader_id: Option<u64>,
}

impl EtcdQuorum {
    /// `statuses` is `None` when the status call failed
    pub fn assess(members: &[EtcdMemberInfo], statuses: Option<&[EtcdMemberStatus]>) -> Self {
        let voters: Vec<&EtcdMemberInfo> = members.iter().filter(|m| !m.is_learner).collect();
        let answered = statuses.filter(|s| !s.is_empty());
        let healthy_ids: Vec<u64> = answered
            .map(|s| {
                voters
                    .iter()
                    .filter(|m| s.iter().any(|st| st.member_id == m.id))
                    .map(|m| m.id)
                    .collect()
            })
            .unwrap_or_default();
        let leader_id = answered.and_then(|s| s.iter().map(|st| st.leader_id).find(|&id| id != 0));
        EtcdQuorum {
            voters: voters.len(),
            healthy: answered.map(|_| healthy_ids.len()),
            quorum_needed: voters.len() / 2 + 1,
            healthy_ids,
            leader_id,
        }
    }

    /// Whether quorum holds now; `None` when unknown
    pub fn has_quorum(&self) -> Option<bool> {
        if self.voters == 0 {
            return None;
        }
        self.healthy.map(|h| h >= self.quorum_needed)
    }

    /// Quorum state for display; `Unknown` when health is unknown
    pub fn state(&self) -> QuorumState {
        match self.healthy {
            Some(healthy) => QuorumState::from_counts(healthy, self.voters),
            None => QuorumState::Unknown,
        }
    }

    /// How many more voters can go down before quorum is lost
    pub fn can_lose(&self) -> Option<usize> {
        self.healthy.map(|h| h.saturating_sub(self.quorum_needed))
    }

    /// Whether `host` (address or URL of a node) is a voting member
    pub fn is_voter(members: &[EtcdMemberInfo], host: &str) -> bool {
        members
            .iter()
            .any(|m| !m.is_learner && m.has_peer_host(host))
    }

    /// Whether the member at `host` is the leader
    pub fn is_leader(&self, members: &[EtcdMemberInfo], host: &str) -> bool {
        self.leader_id.is_some_and(|leader| {
            members
                .iter()
                .any(|m| m.id == leader && m.has_peer_host(host))
        })
    }

    /// Healthy voters left after the node at `host` goes down; `None` when
    /// unknown
    pub fn healthy_after(&self, members: &[EtcdMemberInfo], host: &str) -> Option<usize> {
        let healthy = self.healthy?;
        // A node that is already down doesn't lower the count again
        let node_counts = members
            .iter()
            .any(|m| !m.is_learner && m.has_peer_host(host) && self.healthy_ids.contains(&m.id));
        Some(if node_counts { healthy - 1 } else { healthy })
    }

    /// Whether taking the node at `host` down keeps quorum
    pub fn removal_safety(&self, members: &[EtcdMemberInfo], host: &str) -> SafetyStatus {
        if !Self::is_voter(members, host) {
            return SafetyStatus::Safe;
        }
        let Some(after) = self.healthy_after(members, host) else {
            return SafetyStatus::Unknown;
        };
        if after >= self.quorum_needed {
            SafetyStatus::Safe
        } else if self.voters == 1 {
            SafetyStatus::Unsafe("Single etcd member - quorum will be lost".to_string())
        } else {
            SafetyStatus::Unsafe(format!(
                "Quorum will be lost ({}/{} healthy after, {} needed)",
                after, self.voters, self.quorum_needed
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u64, ip: &str) -> EtcdMemberInfo {
        EtcdMemberInfo {
            id,
            hostname: format!("cp{id}"),
            peer_urls: vec![format!("https://{ip}:2380")],
            client_urls: vec![],
            is_learner: false,
        }
    }

    fn status(member_id: u64, leader_id: u64) -> EtcdMemberStatus {
        EtcdMemberStatus {
            node: String::new(),
            member_id,
            protocol_version: String::new(),
            db_size: 0,
            db_size_in_use: 0,
            leader_id,
            raft_index: 0,
            raft_term: 0,
            raft_applied_index: 0,
            errors: vec![],
            is_learner: false,
        }
    }

    fn three() -> Vec<EtcdMemberInfo> {
        vec![
            member(1, "10.0.0.1"),
            member(2, "10.0.0.2"),
            member(3, "10.0.0.3"),
        ]
    }

    #[test]
    fn all_healthy_reboot_is_safe() {
        let members = three();
        let statuses = [status(1, 1), status(2, 1), status(3, 1)];
        let q = EtcdQuorum::assess(&members, Some(&statuses));
        assert_eq!(q.healthy, Some(3));
        assert_eq!(q.has_quorum(), Some(true));
        assert_eq!(q.can_lose(), Some(1));
        assert_eq!(q.state(), QuorumState::Healthy);
        assert_eq!(q.removal_safety(&members, "10.0.0.2"), SafetyStatus::Safe);
        assert!(q.is_leader(&members, "10.0.0.1"));
        assert!(!q.is_leader(&members, "10.0.0.2"));
    }

    #[test]
    fn one_down_rebooting_another_is_unsafe() {
        let members = three();
        let statuses = [status(1, 1), status(2, 1)];
        let q = EtcdQuorum::assess(&members, Some(&statuses));
        assert_eq!(q.healthy, Some(2));
        assert_eq!(q.can_lose(), Some(0));
        assert!(matches!(
            q.removal_safety(&members, "10.0.0.2"),
            SafetyStatus::Unsafe(_)
        ));
        // The member that is already down can go down "again"
        assert_eq!(q.removal_safety(&members, "10.0.0.3"), SafetyStatus::Safe);
    }

    #[test]
    fn failed_status_call_is_unknown_not_healthy() {
        let members = three();
        for statuses in [None, Some(&[][..])] {
            let q = EtcdQuorum::assess(&members, statuses);
            assert_eq!(q.healthy, None);
            assert_eq!(q.has_quorum(), None);
            assert_eq!(q.can_lose(), None);
            assert_eq!(q.state(), QuorumState::Unknown);
            assert!(matches!(
                q.removal_safety(&members, "10.0.0.1"),
                SafetyStatus::Unknown
            ));
        }
    }

    #[test]
    fn non_member_is_safe_even_when_unknown() {
        let members = three();
        let q = EtcdQuorum::assess(&members, None);
        assert_eq!(q.removal_safety(&members, "10.0.0.9"), SafetyStatus::Safe);
    }

    #[test]
    fn address_prefix_is_not_a_member() {
        let members = vec![member(1, "10.0.0.11")];
        assert!(!EtcdQuorum::is_voter(&members, "10.0.0.1"));
        assert!(EtcdQuorum::is_voter(&members, "10.0.0.11:50000"));
    }

    #[test]
    fn single_member_is_unsafe() {
        let members = vec![member(1, "10.0.0.1")];
        let q = EtcdQuorum::assess(&members, Some(&[status(1, 1)]));
        assert_eq!(
            q.removal_safety(&members, "10.0.0.1"),
            SafetyStatus::Unsafe("Single etcd member - quorum will be lost".to_string())
        );
    }

    #[test]
    fn learners_dont_vote() {
        let mut members = three();
        members.push(EtcdMemberInfo {
            is_learner: true,
            ..member(4, "10.0.0.4")
        });
        let statuses = [status(1, 1), status(2, 1), status(3, 1), status(4, 1)];
        let q = EtcdQuorum::assess(&members, Some(&statuses));
        assert_eq!(q.voters, 3);
        assert_eq!(q.quorum_needed, 2);
        assert_eq!(q.healthy, Some(3));
        assert_eq!(q.removal_safety(&members, "10.0.0.4"), SafetyStatus::Safe);
    }

    #[test]
    fn ipv6_peer_urls() {
        let members = vec![
            member(1, "[fd00::1]"),
            member(2, "[fd00::2]"),
            member(3, "[fd00::3]"),
        ];
        let q = EtcdQuorum::assess(&members, Some(&[status(1, 1), status(2, 1)]));
        assert!(EtcdQuorum::is_voter(&members, "fd00::1"));
        assert!(matches!(
            q.removal_safety(&members, "[fd00::1]:50000"),
            SafetyStatus::Unsafe(_)
        ));
    }

    #[test]
    fn no_members_has_unknown_quorum() {
        let q = EtcdQuorum::assess(&[], Some(&[]));
        assert_eq!(q.has_quorum(), None);
    }
}
