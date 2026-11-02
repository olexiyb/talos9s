//! Packet capture: start/stop, BPF filter, saving to a pcap file

use super::{CaptureResult, CaptureState, NetworkStatsComponent, with_timeout};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::Instant;
use talos_rs::NetDevStats;
use talos9s_core::constants::MAX_CAPTURE_SIZE;

impl NetworkStatsComponent {
    /// Toggle packet capture on/off
    pub(super) fn toggle_capture(&mut self) {
        match &self.capture.state {
            CaptureState::Idle => {
                // Get selected interface name
                if let Some(dev_name) = self.selected_device_name() {
                    self.capture.state = CaptureState::Capturing(dev_name, Instant::now());
                    self.capture.data.clear();
                    // Actual capture start happens in start_capture_async
                }
            }
            CaptureState::Capturing(..) => {
                // Stop capture
                let bytes = self.capture.data.len();
                self.capture.state = CaptureState::Idle;
                self.capture.receiver = None;

                let bytes_str = NetDevStats::format_bytes(bytes as u64);
                self.status_message = Some((
                    format!(
                        "Capture stopped: {} - press 's' to save, open in Wireshark",
                        bytes_str
                    ),
                    Instant::now(),
                ));
            }
        }
    }

    /// Open the packet capture stream in the background (from `refresh`)
    pub(super) fn start_capture_async(&mut self) {
        let CaptureState::Capturing(ref interface, _) = self.capture.state else {
            return;
        };
        if self.capture.receiver.is_some() || self.tasks.capture.is_some() {
            return; // Already started
        }

        let Some(client) = self.client.clone() else {
            self.status_message = Some(("Error: No client configured".to_string(), Instant::now()));
            self.capture.state = CaptureState::Idle;
            return;
        };

        // Use snap_len=65535 to capture full packets
        // Optionally use BPF filter to exclude port 50000 (Talos API)
        // to prevent feedback loop when capturing on management interface
        let interface = interface.clone();
        let use_bpf_filter = self.capture.use_bpf_filter;
        self.tasks.capture = Some(tokio::spawn(async move {
            if use_bpf_filter {
                with_timeout(client.packet_capture_exclude_api(&interface, false, 65535)).await
            } else {
                with_timeout(client.packet_capture(&interface, false, 65535)).await
            }
        }));
    }

    /// Hook up an opened capture stream, unless the capture was stopped
    /// while it opened
    pub(super) fn apply_capture_start(&mut self, result: CaptureResult) {
        let CaptureState::Capturing(ref interface, _) = self.capture.state else {
            return;
        };
        let interface = interface.clone();
        match result {
            Ok(receiver) => {
                self.capture.receiver = Some(receiver);
                let filter_status = if self.capture.use_bpf_filter {
                    " [BPF filter on]"
                } else {
                    " [NO filter]"
                };
                self.status_message = Some((
                    format!(
                        "Capturing on {}{} (save with 's')",
                        interface, filter_status
                    ),
                    Instant::now(),
                ));
            }
            Err(e) => {
                self.status_message = Some((format!("Capture failed: {}", e), Instant::now()));
                self.capture.state = CaptureState::Idle;
            }
        }
    }

    /// Check if capture needs to be started
    pub fn needs_capture_start(&self) -> bool {
        matches!(self.capture.state, CaptureState::Capturing(..)) && self.capture.receiver.is_none()
    }

    /// Poll capture receiver for new data (non-blocking)
    pub fn poll_capture(&mut self) {
        if let Some(ref mut receiver) = self.capture.receiver {
            // Non-blocking receive all available data
            while let Ok(data) = receiver.try_recv() {
                self.capture.data.extend_from_slice(&data);

                // Auto-stop if capture exceeds size limit
                if self.capture.data.len() >= MAX_CAPTURE_SIZE {
                    let bytes_str = NetDevStats::format_bytes(self.capture.data.len() as u64);
                    self.capture.state = CaptureState::Idle;
                    self.capture.receiver = None;
                    self.status_message = Some((
                        format!("Capture auto-stopped at {} - press 's' to save", bytes_str),
                        Instant::now(),
                    ));
                    return;
                }
            }
        }
    }

    /// Check if capture is active
    pub fn is_capturing(&self) -> bool {
        matches!(self.capture.state, CaptureState::Capturing(..))
    }

    /// Save capture data to file (also stops capture if running)
    pub(super) fn save_capture(&mut self) {
        // Get interface name before stopping
        let interface = match &self.capture.state {
            CaptureState::Capturing(iface, ..) => iface.clone(),
            CaptureState::Idle => "capture".to_string(),
        };

        // Stop capture if running
        if self.is_capturing() {
            self.capture.state = CaptureState::Idle;
            self.capture.receiver = None;
        }

        if self.capture.data.is_empty() {
            self.status_message = Some(("No capture data to save".to_string(), Instant::now()));
            return;
        }

        // Generate filename with timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let filename = format!("{}_{}.pcap", interface, timestamp);
        let path = std::path::Path::new("/tmp").join(&filename);

        let bytes_saved = self.capture.data.len();
        // /tmp is shared: create exclusively (never follows a planted symlink)
        // and owner-only, since packet captures can carry credentials.
        let write_result = {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(&path)
                .and_then(|mut file| file.write_all(&self.capture.data))
        };
        match write_result {
            Ok(_) => {
                let size_str = if bytes_saved >= 1_048_576 {
                    format!("{:.1} MB", bytes_saved as f64 / 1_048_576.0)
                } else if bytes_saved >= 1024 {
                    format!("{:.1} KB", bytes_saved as f64 / 1024.0)
                } else {
                    format!("{} B", bytes_saved)
                };
                self.status_message = Some((
                    format!("Saved {} to {}", size_str, path.display()),
                    Instant::now(),
                ));
                // Clear capture data after successful save
                self.capture.data.clear();
            }
            Err(e) => {
                self.status_message = Some((format!("Save failed: {}", e), Instant::now()));
            }
        }
    }

    /// Toggle BPF filter on/off for packet capture
    pub(super) fn toggle_bpf_filter(&mut self) {
        // Only allow toggle when not capturing
        if self.is_capturing() {
            self.status_message = Some((
                "Stop capture first before changing filter setting".to_string(),
                Instant::now(),
            ));
            return;
        }

        self.capture.use_bpf_filter = !self.capture.use_bpf_filter;
        let status = if self.capture.use_bpf_filter {
            "BPF filter ON - excludes API port 50000"
        } else {
            "BPF filter OFF - WARNING: may cause feedback loop on mgmt interface"
        };
        self.status_message = Some((status.to_string(), Instant::now()));
    }

    /// Draw capture status bar at the top of connections view
    pub(super) fn draw_capture_status(&self, frame: &mut Frame, area: Rect) {
        if let CaptureState::Capturing(ref iface, start) = self.capture.state {
            let duration = start.elapsed().as_secs();
            let mins = duration / 60;
            let secs = duration % 60;
            let bytes = self.capture.data.len() as u64;
            let bytes_str = NetDevStats::format_bytes(bytes);

            let status = Line::from(vec![
                Span::styled(
                    " ● ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "REC ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(iface, Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(
                    format!("{:02}:{:02}", mins, secs),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw("  "),
                Span::styled(&bytes_str, Style::default().fg(Color::Blue)),
                Span::raw("  "),
                Span::styled("[c]", Style::default().fg(Color::DarkGray)),
                Span::styled(" stop  ", Style::default().fg(Color::DarkGray)),
                Span::styled("[s]", Style::default().fg(Color::DarkGray)),
                Span::styled(" save to /tmp", Style::default().fg(Color::DarkGray)),
            ]);

            let bar = Paragraph::new(status).style(Style::default().bg(Color::Black));
            frame.render_widget(bar, area);
        }
    }
}
