use sysinfo::{Networks, Pid, ProcessesToUpdate, System};
use std::time::{Duration, Instant};

pub struct SystemStats {
    sys: System,
    networks: Networks,
    pid: Pid,
    last_refresh: Instant,
    refresh_every: Duration,
    pub cpu_pct: f32,
    pub mem_used_gb: f64,
    pub mem_total_gb: f64,
    pub process_cpu_pct: f32,
    pub process_mem_mb: f64,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
}

impl SystemStats {
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_cpu_usage();
        let pid = sysinfo::get_current_pid().unwrap_or(Pid::from_u32(0));
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let networks = Networks::new_with_refreshed_list();

        let mut s = Self {
            sys,
            networks,
            pid,
            last_refresh: Instant::now() - Duration::from_secs(10),
            // Keep off the hot path — sysinfo scans can hitch the UI thread.
            refresh_every: Duration::from_secs(3),
            cpu_pct: 0.0,
            mem_used_gb: 0.0,
            mem_total_gb: 0.0,
            process_cpu_pct: 0.0,
            process_mem_mb: 0.0,
            net_rx_bps: 0.0,
            net_tx_bps: 0.0,
        };
        s.refresh_if_due(true);
        s
    }

    pub fn refresh_if_due(&mut self, force: bool) {
        if !force && self.last_refresh.elapsed() < self.refresh_every {
            return;
        }
        let elapsed = self.last_refresh.elapsed().as_secs_f64().max(0.001);

        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys.refresh_processes(ProcessesToUpdate::Some(&[self.pid]), true);
        self.networks.refresh(true);

        self.cpu_pct = self.sys.global_cpu_usage();
        self.mem_used_gb = self.sys.used_memory() as f64 / (1024.0 * 1024.0 * 1024.0);
        self.mem_total_gb = self.sys.total_memory() as f64 / (1024.0 * 1024.0 * 1024.0);

        if let Some(proc) = self.sys.process(self.pid) {
            self.process_cpu_pct = proc.cpu_usage();
            self.process_mem_mb = proc.memory() as f64 / (1024.0 * 1024.0);
        }

        let mut rx = 0u64;
        let mut tx = 0u64;
        for (name, data) in self.networks.iter() {
            let lname = name.to_ascii_lowercase();
            if lname == "lo" || lname.starts_with("lo:") || lname.starts_with("docker") {
                continue;
            }
            rx = rx.saturating_add(data.received());
            tx = tx.saturating_add(data.transmitted());
        }
        self.net_rx_bps = rx as f64 / elapsed;
        self.net_tx_bps = tx as f64 / elapsed;

        self.last_refresh = Instant::now();
    }

    pub fn summary_line(&self, streams: usize, view: &str, layout: &str) -> String {
        format!(
            "CPU {:>4.0}%  │  RAM {:.1}/{:.1} GiB  │  App {:>4.0}% / {:.0} MiB  │  Net ↓{} ↑{}  │  Streams {}  │  {} ({})",
            self.cpu_pct,
            self.mem_used_gb,
            self.mem_total_gb,
            self.process_cpu_pct,
            self.process_mem_mb,
            format_bps(self.net_rx_bps),
            format_bps(self.net_tx_bps),
            streams,
            view,
            layout,
        )
    }
}

fn format_bps(bps: f64) -> String {
    if bps >= 1_000_000.0 {
        format!("{:.1} MB/s", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.0} KB/s", bps / 1_000.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}
