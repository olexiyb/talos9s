//! The drain options in the Operations start dialog (`plans/operations.md`,
//! "Drain options"): check boxes and values for `kubectl drain`'s flags, each
//! saying what it hits on the checked nodes.

use crate::components::diagnostics::k8s::{DrainOptions, PodImpact};
use crate::operations::{DELAY_BETWEEN_NODES, OpKind};
use crate::ui_ext::{PICK_CURSOR, PICK_DIM, PICK_POINTER, check_box};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::time::Duration;

/// The values `←`/`→` step through
const GRACE_STEPS: [Option<i64>; 6] = [None, Some(0), Some(10), Some(30), Some(60), Some(120)];
const TIMEOUT_STEPS: [Option<u64>; 5] = [Some(60), Some(300), Some(600), Some(1800), None];
const BETWEEN_STEPS: [u64; 5] = [0, 30, 60, 120, 300];
const WAIT_STEPS: [Option<u64>; 5] = [None, Some(300), Some(900), Some(1800), Some(3600)];

/// Label column
const LABEL: usize = 22;

/// What the dialog changes: the drain options and the wait between nodes
#[derive(Debug, Clone)]
pub struct DrainChoice {
    pub options: DrainOptions,
    pub between: Duration,
}

impl Default for DrainChoice {
    fn default() -> Self {
        Self {
            options: DrainOptions::default(),
            between: DELAY_BETWEEN_NODES,
        }
    }
}

/// One option row
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opt {
    IgnoreDaemonsets,
    DeleteEmptydir,
    Force,
    /// Wait for unmanaged pods to finish after cordoning
    WaitToFinish,
    /// The wait covers Job pods
    WaitJobs,
    Grace,
    Timeout,
    Between,
}

impl Opt {
    /// The rows for `kind` on `nodes` nodes: none unless it drains; the wait
    /// between nodes only for a reboot of several
    pub fn shown(kind: OpKind, nodes: usize) -> Vec<Opt> {
        if !kind.drains() {
            return Vec::new();
        }
        let mut rows = vec![
            Opt::IgnoreDaemonsets,
            Opt::DeleteEmptydir,
            Opt::Force,
            Opt::WaitToFinish,
            Opt::WaitJobs,
            Opt::Grace,
            Opt::Timeout,
        ];
        if kind == OpKind::Reboot && nodes > 1 {
            rows.push(Opt::Between);
        }
        rows
    }

    fn label(self) -> &'static str {
        match self {
            Opt::IgnoreDaemonsets => "Ignore DaemonSets",
            Opt::DeleteEmptydir => "Delete emptyDir data",
            Opt::Force => "Force",
            Opt::WaitToFinish => "Wait to finish",
            Opt::WaitJobs => "  also Job pods",
            Opt::Grace => "Grace period",
            Opt::Timeout => "Timeout",
            Opt::Between => "Between nodes",
        }
    }
}

/// What the dialog knows of the pods on the checked nodes
#[derive(Clone, Copy)]
pub enum Preview<'a> {
    Reading,
    Unknown,
    Pods(&'a [PodImpact]),
}

impl DrainChoice {
    /// The choice saved for a context; what is missing is the default
    pub fn from_state(state: &crate::state::DrainState) -> Self {
        let mut c = Self::default();
        let o = &mut c.options;
        o.ignore_daemonsets = state.ignore_daemonsets.unwrap_or(o.ignore_daemonsets);
        o.delete_emptydir_data = state.delete_emptydir_data.unwrap_or(o.delete_emptydir_data);
        o.grace_period_secs = state.grace_period_secs;
        if let Some(t) = state.timeout_secs {
            o.timeout_secs = (t > 0).then_some(t);
        }
        if let Some(w) = state.wait_to_finish_secs {
            o.wait_to_finish_secs = (w > 0).then_some(w);
        }
        o.wait_for_jobs = state.wait_for_jobs.unwrap_or(o.wait_for_jobs);
        if let Some(b) = state.between_nodes_secs {
            c.between = Duration::from_secs(b);
        }
        c
    }

    /// What is saved for `context`: everything but force
    pub fn to_state(&self, context: &str) -> crate::state::DrainState {
        crate::state::DrainState {
            context: context.to_string(),
            ignore_daemonsets: Some(self.options.ignore_daemonsets),
            delete_emptydir_data: Some(self.options.delete_emptydir_data),
            grace_period_secs: self.options.grace_period_secs,
            timeout_secs: Some(self.options.timeout_secs.unwrap_or(0)),
            between_nodes_secs: Some(self.between.as_secs()),
            wait_to_finish_secs: Some(self.options.wait_to_finish_secs.unwrap_or(0)),
            wait_for_jobs: Some(self.options.wait_for_jobs),
        }
    }

    /// Whether a check box is on; `None` for a value
    fn checked(&self, opt: Opt) -> Option<bool> {
        match opt {
            Opt::IgnoreDaemonsets => Some(self.options.ignore_daemonsets),
            Opt::DeleteEmptydir => Some(self.options.delete_emptydir_data),
            Opt::Force => Some(self.options.force),
            Opt::WaitJobs => Some(self.options.wait_for_jobs),
            _ => None,
        }
    }

    /// `Space` or a click: flip a box, or step a value forward
    pub fn toggle(&mut self, opt: Opt) {
        self.step(opt, true);
    }

    /// `←`/`→`: a value's previous or next step; a box flips either way
    pub fn step(&mut self, opt: Opt, forward: bool) {
        let o = &mut self.options;
        match opt {
            Opt::IgnoreDaemonsets => o.ignore_daemonsets = !o.ignore_daemonsets,
            Opt::DeleteEmptydir => o.delete_emptydir_data = !o.delete_emptydir_data,
            Opt::Force => o.force = !o.force,
            Opt::WaitJobs => o.wait_for_jobs = !o.wait_for_jobs,
            Opt::WaitToFinish => {
                o.wait_to_finish_secs = next(&WAIT_STEPS, o.wait_to_finish_secs, forward);
            }
            Opt::Grace => o.grace_period_secs = next(&GRACE_STEPS, o.grace_period_secs, forward),
            Opt::Timeout => o.timeout_secs = next(&TIMEOUT_STEPS, o.timeout_secs, forward),
            Opt::Between => {
                let now = self.between.as_secs();
                self.between = Duration::from_secs(next(&BETWEEN_STEPS, now, forward));
            }
        }
    }

    fn value(&self, opt: Opt) -> String {
        match opt {
            Opt::Grace => self
                .options
                .grace_period_secs
                .map_or("pod default".to_string(), |g| {
                    secs(talos9s_core::convert::non_negative(g))
                }),
            Opt::Timeout => self.options.timeout_secs.map_or("none".to_string(), secs),
            Opt::WaitToFinish => self
                .options
                .wait_to_finish_secs
                .map_or("off".to_string(), |s| format!("up to {}", secs(s))),
            Opt::Between => secs(self.between.as_secs()),
            _ => String::new(),
        }
    }

    /// The option rows, `cursor` on one of `opts`
    pub fn lines(&self, opts: &[Opt], cursor: usize, preview: Preview) -> Vec<Line<'static>> {
        opts.iter()
            .enumerate()
            .map(|(i, &opt)| {
                let here = i == cursor;
                let mut spans = vec![if here {
                    Span::styled(format!(" {PICK_POINTER} "), PICK_CURSOR)
                } else {
                    Span::raw("   ")
                }];
                let label = format!("{:<LABEL$}", opt.label());
                match self.checked(opt) {
                    Some(on) => {
                        spans.push(check_box(on));
                        let style = match (here, on) {
                            (true, _) => PICK_CURSOR,
                            (false, true) => Style::default(),
                            (false, false) => PICK_DIM,
                        };
                        spans.push(Span::styled(label, style));
                        spans.push(note(opt, on, preview));
                    }
                    None => {
                        spans.push(Span::raw("    "));
                        spans.push(Span::styled(
                            label,
                            if here { PICK_CURSOR } else { Style::default() },
                        ));
                        spans.push(Span::styled(
                            self.value(opt),
                            Style::default().fg(Color::Cyan),
                        ));
                        if here {
                            spans.push(Span::styled("  ←/→", PICK_DIM));
                        }
                        if opt == Opt::WaitToFinish
                            && let Preview::Pods(pods) = preview
                        {
                            let n = pods
                                .iter()
                                .filter(|p| p.waits_to_finish(self.options.wait_for_jobs))
                                .count();
                            if n > 0 {
                                spans.push(Span::styled(
                                    format!("  {} could end by themselves", pods_word(n)),
                                    PICK_DIM,
                                ));
                            }
                        }
                    }
                }
                Line::from(spans)
            })
            .collect()
    }
}

impl Opt {
    /// Whether this option decides about `pod`: the table marks those rows
    pub fn hits(self, pod: &PodImpact) -> bool {
        match self {
            Opt::IgnoreDaemonsets => pod.daemonset,
            Opt::DeleteEmptydir => pod.emptydir && !pod.daemonset,
            Opt::Force => pod.owner.is_none() && !pod.finished,
            Opt::WaitToFinish => pod.waits_to_finish(true),
            Opt::WaitJobs => pod.waits_to_finish(true) && pod.owner.is_some(),
            _ => false,
        }
    }
}

/// What a check box hits on the checked nodes: a count, the table below
/// names the pods
fn note(opt: Opt, on: bool, preview: Preview) -> Span<'static> {
    let pods = match preview {
        Preview::Reading => return Span::styled("reading pods…", PICK_DIM),
        Preview::Unknown => return Span::styled("pods unknown", PICK_DIM),
        Preview::Pods(pods) => pods,
    };
    let n = pods.iter().filter(|p| opt.hits(p)).count();
    if n == 0 {
        return Span::styled("none here", PICK_DIM);
    }
    let pods = pods_word(n);
    match (opt, on) {
        (Opt::WaitJobs, true) => Span::styled(
            format!("{pods} of Jobs wait to finish"),
            Style::default().fg(Color::Yellow),
        ),
        (Opt::WaitJobs, false) => Span::styled(
            format!("{pods} of Jobs evicted, their runs restart"),
            PICK_DIM,
        ),
        (_, false) => Span::styled(
            format!("stops the drain: {pods}"),
            Style::default().fg(Color::Red),
        ),
        (Opt::IgnoreDaemonsets, true) => Span::styled(format!("{pods} stay"), PICK_DIM),
        (Opt::DeleteEmptydir, true) => Span::styled(
            format!("{pods} lose emptyDir data"),
            Style::default().fg(Color::Yellow),
        ),
        (_, true) => Span::styled(
            format!("{pods} gone for good"),
            Style::default().fg(Color::Yellow),
        ),
    }
}

fn pods_word(n: usize) -> String {
    talos9s_core::pluralize(n, "pod", "pods")
}

/// The step after (or before) `now`; one not in `steps` goes to the first
fn next<T: PartialEq + Copy>(steps: &[T], now: T, forward: bool) -> T {
    let j = match steps.iter().position(|s| *s == now) {
        None => 0,
        Some(i) if forward => (i + 1).min(steps.len().saturating_sub(1)),
        Some(i) => i.saturating_sub(1),
    };
    steps.get(j).copied().unwrap_or(now)
}

/// `30s`, `5m`
pub fn secs(s: u64) -> String {
    if s > 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_the_operation() {
        assert!(Opt::shown(OpKind::Cordon, 1).is_empty());
        assert_eq!(Opt::shown(OpKind::Drain, 3).len(), 7);
        assert_eq!(Opt::shown(OpKind::Reboot, 1).len(), 7);
        assert_eq!(Opt::shown(OpKind::Reboot, 2).last(), Some(&Opt::Between));
    }

    #[test]
    fn values_step_and_stop_at_the_ends() {
        let mut c = DrainChoice::default();
        assert_eq!(c.value(Opt::Grace), "pod default");
        c.step(Opt::Grace, false);
        assert_eq!(c.value(Opt::Grace), "pod default");
        c.step(Opt::Grace, true);
        c.step(Opt::Grace, true);
        assert_eq!(c.options.grace_period_secs, Some(10));
        assert_eq!(c.value(Opt::Timeout), "5m");
        for _ in 0..9 {
            c.toggle(Opt::Timeout);
        }
        assert_eq!(c.options.timeout_secs, None);
        assert_eq!(c.value(Opt::Timeout), "none");
        c.step(Opt::Between, true);
        assert_eq!(c.value(Opt::Between), "1m");
        c.toggle(Opt::Force);
        assert!(c.options.force);
        c.step(Opt::Force, false);
        assert!(!c.options.force);
    }

    /// Saved and read back the same, except force
    #[test]
    fn saved_per_context_without_force() {
        let mut c = DrainChoice::default();
        c.options.force = true;
        c.options.delete_emptydir_data = false;
        c.options.timeout_secs = None;
        c.options.grace_period_secs = Some(30);
        c.between = Duration::from_secs(60);
        let back = DrainChoice::from_state(&c.to_state("bc"));
        assert!(!back.options.force);
        assert!(!back.options.delete_emptydir_data);
        assert_eq!(back.options.timeout_secs, None);
        assert_eq!(back.options.grace_period_secs, Some(30));
        assert_eq!(back.between, Duration::from_secs(60));
        let empty = DrainChoice::from_state(&crate::state::DrainState::default());
        assert_eq!(empty.options.timeout_secs, Some(300));
        assert!(empty.options.delete_emptydir_data);
    }

    #[test]
    fn notes_count_what_each_box_hits() {
        let pod = |name: &str, owner: Option<&str>, emptydir: bool| PodImpact {
            node: "n1".into(),
            pod: name.into(),
            owner: owner.map(Into::into),
            daemonset: owner == Some("DaemonSet"),
            emptydir,
            finished: false,
            labels: Default::default(),
            pdb: None,
        };
        let pods = [
            pod("ns/cilium", Some("DaemonSet"), false),
            pod("ns/debug", None, false),
            pod("ns/c-0", Some("StatefulSet"), true),
            pod("ns/c-1", Some("StatefulSet"), true),
        ];
        let p = Preview::Pods(&pods);
        assert_eq!(note(Opt::IgnoreDaemonsets, true, p).content, "1 pod stay");
        assert_eq!(
            note(Opt::DeleteEmptydir, true, p).content,
            "2 pods lose emptyDir data"
        );
        let stop = note(Opt::Force, false, p);
        assert_eq!(stop.content, "stops the drain: 1 pod");
        assert_eq!(stop.style.fg, Some(Color::Red));
        assert_eq!(
            note(Opt::Force, true, Preview::Unknown).content,
            "pods unknown"
        );
        assert_eq!(
            note(Opt::Force, false, Preview::Pods(&[])).content,
            "none here"
        );
    }
}
