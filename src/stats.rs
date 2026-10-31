// Ping statistics shared between the capture threads and the overlay.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering::Relaxed};

use crate::settings;
use crate::util::{RawIp, ip_text, local_timestamp, tick_ms};

/// Server ip/port of a connection.
pub type Endpoint = (RawIp, u16);

// The game sends a ping every ~10 s.
pub const WINDOW_MS: i64 = 60_000; // window for the in-game ping median/max
pub const STALE_MS: i64 = 25_000; // no new samples for longer than this — show "—"

pub const SPIKE_SHOW_MS: i64 = 60_000; // how long a spike stays on screen
pub const SPIKE_FLASH_MS: i64 = 10_000; // how long it is highlighted in red
const JUMP_RATIO: f64 = 1.6;
const JUMP_MIN_MS: f64 = 40.0;

/// The server sends data every 50 ms (20 Hz tick). A gap longer than FREEZE_MS with no data at all is a freeze
/// (the server stalled, even if the network is fine).
pub const FREEZE_MS: f64 = 200.0;

const NO_ENDPOINT: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq)]
pub enum SpikeKind {
    Game,
    Network,
    Loss,
    Freeze,
}

#[derive(Clone, Copy)]
struct Spike {
    at: i64,
    value: f64,
    kind: SpikeKind,
    cause: Option<&'static str>,
}

/// Problems in the last minute: the worst of each kind and ms since the most recent one.
pub struct SpikeSummary {
    pub game_max: Option<f64>,
    pub game_cause: Option<&'static str>,
    pub network_max: Option<f64>,
    pub losses: usize,
    pub freeze_max: Option<f64>,
    pub since_last_ms: i64,
}

/// Diagnostics: a data packet on the game connection (--gaps mode only).
#[derive(Clone, Copy)]
pub struct TracePoint {
    pub at: f64,
    pub inbound: bool,
}

#[derive(Default)]
struct Inner {
    samples: VecDeque<(i64, f64)>,
    last: Option<(i64, f64)>,
    log: Option<File>,

    // continuous TCP RTT (network round-trip)
    tcp: VecDeque<(i64, f64)>,

    spikes: Vec<Spike>,
    net_second_mins: VecDeque<f64>, // min RTT for each second, last 60
    net_second: i64,
    net_second_min: f64,

    last_gateway: Option<Endpoint>,
}

pub struct PingStats {
    inner: Mutex<Inner>,
    game_endpoint: AtomicU64,

    pub packets: AtomicI64,
    pub pings: AtomicI64,
    pub tcp_samples: AtomicI64,
    pub losses: AtomicI64,
    pub freezes: AtomicI64,
    pub gateway_changes: AtomicI64,
    pub threshold_ms: AtomicI32,

    status: Mutex<String>,
    debug: Mutex<String>,

    /// Timestamps of data packets on the game connection (--gaps mode only).
    pub trace: Option<Mutex<Vec<TracePoint>>>,
}

impl PingStats {
    pub fn new(trace: bool) -> Self {
        PingStats {
            inner: Mutex::new(Inner { net_second: -1, ..Default::default() }),
            game_endpoint: AtomicU64::new(NO_ENDPOINT),
            packets: AtomicI64::new(0),
            pings: AtomicI64::new(0),
            tcp_samples: AtomicI64::new(0),
            losses: AtomicI64::new(0),
            freezes: AtomicI64::new(0),
            gateway_changes: AtomicI64::new(0),
            threshold_ms: AtomicI32::new(100),
            status: Mutex::new("starting…".into()),
            debug: Mutex::new(String::new()),
            trace: trace.then(|| Mutex::new(Vec::new())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self) -> String {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = s.into();
    }

    pub fn debug(&self) -> String {
        self.debug.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_debug(&self, s: String) {
        *self.debug.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    /// The connection the game's ping was found on (server ip/port).
    pub fn game_endpoint(&self) -> Option<Endpoint> {
        let v = self.game_endpoint.load(Relaxed);
        (v != NO_ENDPOINT).then_some(((v >> 16) as u32, v as u16))
    }

    pub fn set_game_endpoint(&self, value: Option<Endpoint>) {
        // Count how many times pings started arriving from a different gateway (the first one after start is not a change).
        if let Some(v) = value {
            let mut inner = self.lock();
            if inner.last_gateway.is_some_and(|last| last != v) {
                self.gateway_changes.fetch_add(1, Relaxed);
            }
            inner.last_gateway = Some(v);
        }
        let raw = value.map_or(NO_ENDPOINT, |(ip, port)| (ip as u64) << 16 | port as u64);
        self.game_endpoint.store(raw, Relaxed);
    }

    /// Address of the game gateway the client is connected to, e.g. "203.0.113.42:12000".
    pub fn game_endpoint_text(&self) -> Option<String> {
        self.game_endpoint().map(|(ip, port)| format!("{}:{port}", ip_text(ip)))
    }

    pub fn ms_since_last_ping(&self) -> i64 {
        match self.lock().last {
            Some((at, _)) => tick_ms() - at,
            None => i64::MAX,
        }
    }

    /// `pc_ms`: how long the request sat on the PC before being sent (if it could be determined).
    pub fn add(&self, rtt: f64, pc_ms: Option<f64>) {
        let now = tick_ms();
        let gateway = self.game_endpoint_text().unwrap_or_default();
        let threshold = self.threshold_ms.load(Relaxed) as f64;
        {
            let mut s = self.lock();
            trim(&mut s, now);
            // Breakdown: PC (before sending) + network (current TCP RTT) + server (the rest).
            let pc = pc_ms.unwrap_or(0.0);
            let net = s.tcp.iter().map(|t| t.1).fold(f64::INFINITY, f64::min);
            let net = if net.is_finite() { net } else { 0.0 };
            let server = (rtt - pc - net).max(0.0);
            let cause = if pc >= net && pc >= server {
                "PC"
            } else if net >= server {
                "network"
            } else {
                "server"
            };
            detect_game_spike(&mut s, now, rtt, cause, threshold);
            s.last = Some((now, rtt));
            s.samples.push_back((now, rtt));
            trim(&mut s, now);
            if let Some(log) = s.log.as_mut() {
                let pc_text = pc_ms.map(|p| format!("{p:.1}")).unwrap_or_default();
                let _ = writeln!(log, "{},{rtt:.1},{pc_text},{net:.1},{server:.1},{gateway}", local_timestamp());
            }
        }
        self.pings.fetch_add(1, Relaxed);
    }

    pub fn add_tcp(&self, rtt: f64) {
        let now = tick_ms();
        let threshold = self.threshold_ms.load(Relaxed) as f64;
        {
            let mut s = self.lock();
            detect_network_spike(&mut s, now, rtt, threshold);
            s.tcp.push_back((now, rtt));
            trim_tcp(&mut s, now);
        }
        self.tcp_samples.fetch_add(1, Relaxed);
    }

    pub fn add_freeze(&self, gap_ms: f64) {
        let now = tick_ms();
        let gateway = self.game_endpoint_text().unwrap_or_default();
        {
            let mut s = self.lock();
            s.spikes.push(Spike { at: now, value: gap_ms, kind: SpikeKind::Freeze, cause: None });
            if let Some(log) = s.log.as_mut() {
                let _ = writeln!(log, "{},freeze,{gap_ms:.0},,,,{gateway}", local_timestamp());
            }
        }
        self.freezes.fetch_add(1, Relaxed);
    }

    /// A retransmission on the game connection = a lost packet.
    pub fn add_loss(&self) {
        let now = tick_ms();
        self.lock().spikes.push(Spike { at: now, value: 0.0, kind: SpikeKind::Loss, cause: None });
        self.losses.fetch_add(1, Relaxed);
    }

    pub fn spike_snapshot(&self) -> Option<SpikeSummary> {
        let now = tick_ms();
        let mut s = self.lock();
        s.spikes.retain(|x| now - x.at <= SPIKE_SHOW_MS);
        let last = s.spikes.last()?;
        let max = |k: SpikeKind| s.spikes.iter().filter(|x| x.kind == k).map(|x| x.value).reduce(f64::max);
        // The first of the worst game spikes (like OrderByDescending().FirstOrDefault()).
        let mut worst_game: Option<&Spike> = None;
        for x in s.spikes.iter().filter(|x| x.kind == SpikeKind::Game) {
            if worst_game.is_none_or(|w| x.value > w.value) {
                worst_game = Some(x);
            }
        }
        Some(SpikeSummary {
            game_max: max(SpikeKind::Game),
            game_cause: worst_game.and_then(|w| w.cause),
            network_max: max(SpikeKind::Network),
            losses: s.spikes.iter().filter(|x| x.kind == SpikeKind::Loss).count(),
            freeze_max: max(SpikeKind::Freeze),
            since_last_ms: now - last.at,
        })
    }

    /// Min and median TCP RTT over the last 2 seconds.
    pub fn tcp_snapshot(&self) -> (Option<f64>, Option<f64>) {
        let now = tick_ms();
        let mut s = self.lock();
        trim_tcp(&mut s, now);
        if s.tcp.is_empty() {
            return (None, None);
        }
        let a = sorted(s.tcp.iter().map(|t| t.1));
        (Some(a[0]), Some(a[a.len() / 2]))
    }

    /// (current, median, max) of the in-game ping; current is None when there is no fresh data.
    pub fn snapshot(&self) -> (Option<f64>, f64, f64) {
        let now = tick_ms();
        let mut s = self.lock();
        trim(&mut s, now);
        let Some((last_at, last_rtt)) = s.last else { return (None, 0.0, 0.0) };
        if s.samples.is_empty() || now - last_at > STALE_MS {
            return (None, 0.0, 0.0);
        }
        let a = sorted(s.samples.iter().map(|t| t.1));
        (Some(last_rtt), a[a.len() / 2], a[a.len() - 1])
    }

    /// Turns the CSV log on or off; returns the log path when it is on.
    pub fn set_logging(&self, enabled: bool) -> Option<PathBuf> {
        let mut s = self.lock();
        s.log = None;
        if !enabled {
            return None;
        }
        let path = settings::dir().join("ping-log.csv");
        let fresh = std::fs::metadata(&path).map_or(true, |m| m.len() == 0);
        let mut f = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        if fresh {
            let _ = writeln!(f, "time,ping_ms,pc_ms,network_ms,server_ms,gateway");
        }
        s.log = Some(f);
        Some(path)
    }

    pub fn is_logging(&self) -> bool {
        self.lock().log.is_some()
    }

    /// Records a game ping together with how long the request sat on the PC (`pc`).
    pub fn add_ping(&self, rtt: f64, pc: Option<f64>) {
        self.add(rtt, pc);
        self.set_debug(match pc {
            Some(d) => format!("pc={d:.2}ms"),
            None => "pc=?".into(),
        });
    }
}

fn sorted(it: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut a: Vec<f64> = it.collect();
    a.sort_by(f64::total_cmp);
    a
}

fn trim(s: &mut Inner, now: i64) {
    while s.samples.front().is_some_and(|x| now - x.0 > WINDOW_MS) {
        s.samples.pop_front();
    }
}

fn trim_tcp(s: &mut Inner, now: i64) {
    while s.tcp.front().is_some_and(|x| now - x.0 > 2000) {
        s.tcp.pop_front();
    }
}

/// In-game ping above the threshold or sharply above normal (median of the last minute).
fn detect_game_spike(s: &mut Inner, now: i64, rtt: f64, cause: &'static str, threshold: f64) {
    let mut baseline = 0.0;
    if s.samples.len() >= 3 {
        let a = sorted(s.samples.iter().map(|x| x.1));
        baseline = a[a.len() / 2];
    }
    let jump = baseline > 0.0 && rtt >= baseline * JUMP_RATIO && rtt - baseline >= JUMP_MIN_MS;
    if rtt >= threshold || jump {
        s.spikes.push(Spike { at: now, value: rtt, kind: SpikeKind::Game, cause: Some(cause) });
    }
}

/// Network: take the min RTT of each second (it is unaffected by delayed ACKs).
/// If a second ended noticeably above the usual minimum, that is a network spike.
fn detect_network_spike(s: &mut Inner, now: i64, rtt: f64, threshold: f64) {
    let sec = now / 1000;
    if sec == s.net_second {
        if rtt < s.net_second_min {
            s.net_second_min = rtt;
        }
        return;
    }
    if s.net_second >= 0 {
        let done = s.net_second_min;
        if s.net_second_mins.len() >= 5 {
            let baseline = s.net_second_mins.iter().copied().fold(f64::INFINITY, f64::min);
            if done - baseline >= JUMP_MIN_MS || done >= threshold {
                s.spikes.push(Spike { at: now, value: done, kind: SpikeKind::Network, cause: None });
            }
        }
        s.net_second_mins.push_back(done);
        if s.net_second_mins.len() > 60 {
            s.net_second_mins.pop_front();
        }
    }
    s.net_second = sec;
    s.net_second_min = rtt;
}
