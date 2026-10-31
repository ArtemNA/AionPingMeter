// Finds the AION2.exe connections and captures their traffic with Npcap.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_ALL};
use windows_sys::Win32::Networking::WinSock::AF_INET;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

use crate::parser::{self, ClockCalibrator, RecentSet, TcpRttTracker};
use crate::pcap::{self, Capture};
use crate::stats::{Endpoint, FREEZE_MS, PingStats, STALE_MS, TracePoint};
use crate::util::{RawIp, ip_text, is_loopback, qpc_ms};

const REFRESH_MS: u64 = 4000;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameConnection {
    pub local: RawIp,
    pub remote: RawIp,
    pub remote_port: u16,
}

/// Every few seconds finds AION2.exe connections and keeps capture limited to them.
pub struct CaptureManager {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureManager {
    pub fn start(stats: Arc<PingStats>) -> CaptureManager {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name("capture-manager".into())
            .spawn(move || {
                let mut state = ManagerState { stats, captures: HashMap::new(), last_signature: String::new() };
                while !flag.load(Relaxed) {
                    if let Err(e) = state.refresh() {
                        state.stats.set_status(format!("error: {e}"));
                    }
                    thread::park_timeout(Duration::from_millis(REFRESH_MS));
                }
                state.stop_all();
            })
            .expect("spawn capture manager");
        CaptureManager { stop, thread: Some(thread) }
    }
}

impl Drop for CaptureManager {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

struct ManagerState {
    stats: Arc<PingStats>,
    captures: HashMap<String, DeviceCapture>,
    last_signature: String,
}

impl ManagerState {
    fn refresh(&mut self) -> Result<(), String> {
        let mut conns = find_game_connections();
        if conns.is_empty() {
            self.stop_all();
            self.last_signature.clear();
            self.stats.set_status("game not found");
            return Ok(());
        }

        // Once we know which connection carries the game's ping, listen to that one only.
        // If there has been no ping on it for a while (the game moved to another server but kept the old connection),
        // forget it and listen to all game connections again.
        if self.stats.game_endpoint().is_some() && self.stats.ms_since_last_ping() > STALE_MS {
            self.stats.set_game_endpoint(None);
        }
        if let Some(g) = self.stats.game_endpoint()
            && conns.iter().any(|c| is_endpoint(c, g)) {
                conns.retain(|c| is_endpoint(c, g));
            }

        let mut parts: Vec<String> =
            conns.iter().map(|c| format!("{}>{}:{}", ip_text(c.local), ip_text(c.remote), c.remote_port)).collect();
        parts.sort();
        let signature = parts.join(";");
        let all_alive = !self.captures.is_empty() && self.captures.values().all(|c| c.alive());
        if signature == self.last_signature && all_alive {
            return Ok(());
        }
        self.last_signature = signature;

        let devices = pcap::list_devices()?;
        let mut wanted: HashMap<String, Vec<GameConnection>> = HashMap::new();
        for c in &conns {
            let device = if is_loopback(c.remote) {
                devices.iter().find(|(n, _)| n.to_ascii_lowercase().contains("loopback"))
            } else {
                devices.iter().find(|(_, addrs)| addrs.contains(&c.local))
            };
            if let Some((name, _)) = device {
                wanted.entry(name.clone()).or_default().push(*c);
            }
        }

        let gone: Vec<String> = self.captures.keys().filter(|k| !wanted.contains_key(*k)).cloned().collect();
        for name in gone {
            if let Some(c) = self.captures.remove(&name) {
                c.stop();
            }
        }

        if wanted.is_empty() {
            self.stats.set_status("adapter not found");
            self.last_signature.clear();
            return Ok(());
        }

        for (device, list) in wanted {
            // Only game connection traffic — the driver drops everything else.
            let mut clauses: Vec<String> = Vec::new();
            for c in &list {
                let clause = format!("(host {} and port {})", ip_text(c.remote), c.remote_port);
                if !clauses.contains(&clause) {
                    clauses.push(clause);
                }
            }
            let filter = format!("tcp and ({})", clauses.join(" or "));
            let remotes: HashSet<Endpoint> = list.iter().map(|c| (c.remote, c.remote_port)).collect();
            let target = Arc::new(Target { filter, remotes });
            match self.captures.get(&device) {
                Some(cap) if cap.alive() => cap.set_target(target),
                _ => {
                    if let Some(old) = self.captures.remove(&device) {
                        old.stop();
                    }
                    let cap = DeviceCapture::start(device.clone(), target, self.stats.clone());
                    self.captures.insert(device, cap);
                }
            }
        }
        self.stats.set_status("waiting for ping…");
        Ok(())
    }

    fn stop_all(&mut self) {
        for (_, c) in self.captures.drain() {
            c.stop();
        }
    }
}

fn is_endpoint(c: &GameConnection, e: Endpoint) -> bool {
    c.remote == e.0 && c.remote_port == e.1
}

struct Target {
    filter: String,
    remotes: HashSet<Endpoint>,
}

/// One capture thread per network adapter.
struct DeviceCapture {
    target: Arc<Mutex<Arc<Target>>>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl DeviceCapture {
    fn start(device: String, target: Arc<Target>, stats: Arc<PingStats>) -> DeviceCapture {
        let target = Arc::new(Mutex::new(target));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, s) = (target.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
                if let Err(e) = run_capture(&device, &t, &s, &stats) {
                    stats.set_status(e);
                }
            })
            .expect("spawn capture");
        DeviceCapture { target, stop, thread }
    }

    fn alive(&self) -> bool {
        !self.thread.is_finished()
    }

    fn set_target(&self, target: Arc<Target>) {
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = target;
    }

    /// Asks the thread to stop and waits up to a second for it (it wakes every 250 ms).
    fn stop(self) {
        self.stop.store(true, Relaxed);
        for _ in 0..20 {
            if self.thread.is_finished() {
                let _ = self.thread.join();
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn run_capture(device: &str, target: &Mutex<Arc<Target>>, stop: &AtomicBool, stats: &PingStats) -> Result<(), String> {
    let p = Capture::create(device).map_err(|e| format!("Npcap: {e}"))?;
    p.configure_and_activate().map_err(|e| format!("Npcap: {e}"))?;

    let link_type = p.datalink();
    let mut clock = ClockCalibrator::new();
    let mut seen = RecentSet::new(256);
    let mut trackers: HashMap<Endpoint, TcpRttTracker> = HashMap::new();
    let mut last_server_data = 0.0; // when data last arrived from the game server
    let mut out_times = [0.0f64; 256];
    let mut out_idx = 0;
    let mut applied: Option<Arc<Target>> = None;

    while !stop.load(Relaxed) {
        let target = target.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !applied.as_ref().is_some_and(|a| Arc::ptr_eq(a, &target)) {
            if !p.set_filter(&target.filter) {
                return Err(format!("filter: {}", p.error()));
            }
            applied = Some(target.clone());
            trackers.clear();
            last_server_data = 0.0;
        }

        let (rc, packet) = p.next();
        if rc == 0 {
            continue; // timeout — just re-check stop/filter
        }
        if rc < 0 {
            return Err(format!("Npcap: {}", p.error()));
        }
        let Some((header, frame)) = packet else { continue };

        let received_ms = qpc_ms();
        stats.packets.fetch_add(1, Relaxed);

        let arrival_ms = clock.qpc_ms_of(header.sec as f64 * 1000.0 + header.usec as f64 / 1000.0, received_ms);
        let Some(seg) = parser::parse_tcp(frame, link_type) else { continue };

        let inbound = target.remotes.contains(&(seg.src_ip, seg.src_port));
        let key = if inbound { (seg.src_ip, seg.src_port) } else { (seg.dst_ip, seg.dst_port) };
        let game = stats.game_endpoint();
        let tracker = trackers.entry(key).or_default();

        if let Some(trace) = &stats.trace
            && seg.payload_length > 0 && game == Some(key) {
                trace.lock().unwrap_or_else(|e| e.into_inner()).push(TracePoint { at: arrival_ms, inbound });
            }

        if !inbound {
            if seg.payload_length > 0 {
                out_times[out_idx] = arrival_ms;
                out_idx = (out_idx + 1) % out_times.len();
            }
            if tracker.on_outbound(&seg, arrival_ms) && game == Some(key) {
                stats.add_loss();
            }
            continue;
        }

        if seg.payload_length > 0 {
            let payload = &frame[seg.payload_offset..seg.payload_offset + seg.payload_length];
            // How long the request sat on your PC: from the game's timestamp to our first packet on the wire after it.
            let found = parser::scan(payload, arrival_ms, &mut seen, |sent_stamp_ms, rtt| {
                let mut best = f64::MAX;
                for &t in &out_times {
                    if t >= sent_stamp_ms - 2.0 && t - sent_stamp_ms < best {
                        best = t - sent_stamp_ms;
                    }
                }
                let pc = (best <= rtt).then(|| best.max(0.0));
                stats.add_ping(rtt, pc);
            });
            if found {
                stats.set_game_endpoint(Some((seg.src_ip, seg.src_port)));
            }
        }
        let game = stats.game_endpoint();

        if seg.payload_length > 0 && game == Some(key) {
            if last_server_data > 0.0 && arrival_ms - last_server_data > FREEZE_MS {
                stats.add_freeze(arrival_ms - last_server_data);
            }
            last_server_data = f64::max(last_server_data, arrival_ms);
        }

        if tracker.is_inbound_retransmit(&seg) && game == Some(key) {
            stats.add_loss();
        }

        if let Some(r) = tracker.on_inbound(&seg, arrival_ms)
            && game == Some((seg.src_ip, seg.src_port)) {
                stats.add_tcp(r);
            }
    }
    Ok(())
}

/// ESTABLISHED IPv4 TCP connections of every AION2.exe process.
pub fn find_game_connections() -> Vec<GameConnection> {
    let pids = game_pids();
    let mut result = Vec::new();
    if pids.is_empty() {
        return result;
    }

    unsafe {
        let mut size = 0u32;
        GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0);
        if size == 0 {
            return result;
        }
        // u32 elements keep the table aligned.
        let mut buf = vec![0u32; (size as usize).div_ceil(4)];
        if GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0) != 0 {
            return result;
        }
        let count = buf[0] as usize;
        for i in 0..count {
            // MIB_TCPROW_OWNER_PID: state, local addr, local port, remote addr, remote port, pid
            let Some(row) = buf.get(1 + i * 6..1 + i * 6 + 6) else { break };
            if row[0] != 5 {
                continue; // ESTABLISHED
            }
            if !pids.contains(&row[5]) {
                continue;
            }
            let raw_port = row[4];
            let port = (((raw_port & 0xFF) << 8) | ((raw_port >> 8) & 0xFF)) as u16;
            result.push(GameConnection { local: row[1], remote: row[3], remote_port: port });
        }
    }
    result
}

fn game_pids() -> HashSet<u32> {
    let mut pids = HashSet::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return pids;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]);
            if name.eq_ignore_ascii_case("AION2.exe") {
                pids.insert(e.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    pids
}
