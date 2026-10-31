// Packet parsing: TCP segments out of captured frames, game ping replies out of TCP payloads,
// plus the per-connection RTT tracker and the Npcap→QPC clock calibration.

use std::collections::{HashSet, VecDeque};

use crate::util::RawIp;

// Client clock on Global: QPC in ms + 2^24 s.
pub const CLIENT_CLOCK_BASE: i64 = 16_777_216_000;
const CLIENT_CLOCK_LIMIT: i64 = 62_135_596_800_000; // above this it is UTC (KR/TW), not our format
const MAX_RTT_MS: f64 = 3000.0;

const SIGNATURE: [u8; 4] = [0x03, 0x36, 0x00, 0x00]; // opcode 822 + 00 00

/// Looks for ping replies in a TCP payload; calls on_ping(sent_at_ms, rtt) for each new one.
/// Returns true if at least one was found.
pub fn scan(payload: &[u8], arrival_qpc_ms: f64, seen: &mut RecentSet, mut on_ping: impl FnMut(f64, f64)) -> bool {
    let mut found = false;
    let mut pos = 0;
    loop {
        let Some(idx) = payload[pos..].windows(4).position(|w| w == SIGNATURE) else { return found };
        let at = pos + idx;
        pos = at + 1;
        if at + 12 > payload.len() {
            return found;
        }

        let stamp = i64::from_le_bytes(payload[at + 4..at + 12].try_into().unwrap());
        if !(CLIENT_CLOCK_BASE..CLIENT_CLOCK_LIMIT).contains(&stamp) {
            continue;
        }

        let rtt = arrival_qpc_ms + CLIENT_CLOCK_BASE as f64 - stamp as f64;
        if !(0.0..=MAX_RTT_MS).contains(&rtt) {
            continue;
        }
        found = true;
        if !seen.add(stamp) {
            continue; // TCP retransmission of the same reply
        }

        on_ping((stamp - CLIENT_CLOCK_BASE) as f64, rtt);
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct TcpSegment {
    pub src_ip: RawIp,
    pub dst_ip: RawIp,
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub has_ack: bool,
    pub payload_offset: usize,
    pub payload_length: usize,
}

fn be16(f: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([f[at], f[at + 1]])
}

fn be32(f: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(f[at..at + 4].try_into().unwrap())
}

fn le32(f: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(f[at..at + 4].try_into().unwrap())
}

/// Parses a TCP segment in a frame (Ethernet / loopback / raw IP).
pub fn parse_tcp(frame: &[u8], link_type: i32) -> Option<TcpSegment> {
    let ip = match link_type {
        1 => {
            // Ethernet
            if frame.len() < 14 {
                return None;
            }
            let mut ip = 14;
            let mut ether_type = be16(frame, 12);
            if ether_type == 0x8100 && frame.len() >= 18 {
                ether_type = be16(frame, 16);
                ip = 18;
            }
            if ether_type != 0x0800 && ether_type != 0x86DD {
                return None;
            }
            ip
        }
        0 => 4,               // Npcap loopback (BSD null)
        12 | 14 | 101 => 0,   // raw IP (some VPN adapters)
        _ => return None,
    };
    if frame.len() < ip + 20 {
        return None;
    }

    let version = frame[ip] >> 4;
    let (tcp, mut ip_end, src, dst);
    if version == 4 {
        let ihl = (frame[ip] & 0x0F) as usize * 4;
        if frame[ip + 9] != 6 || ihl < 20 {
            return None;
        }
        ip_end = ip + be16(frame, ip + 2) as usize;
        tcp = ip + ihl;
        src = le32(frame, ip + 12); // same byte order as the Win32 tables (RawIp)
        dst = le32(frame, ip + 16);
    } else if version == 6 {
        if frame.len() < ip + 40 || frame[ip + 6] != 6 {
            return None;
        }
        ip_end = ip + 40 + be16(frame, ip + 4) as usize;
        tcp = ip + 40;
        src = 0;
        dst = 0;
    } else {
        return None;
    }

    // Frames with hardware segment coalescing (LSO/RSC) may have total length = 0.
    if ip_end <= tcp || ip_end > frame.len() {
        ip_end = frame.len();
    }
    if tcp + 20 > ip_end {
        return None;
    }

    let payload = tcp + (frame[tcp + 12] >> 4) as usize * 4;
    Some(TcpSegment {
        src_ip: src,
        dst_ip: dst,
        src_port: be16(frame, tcp),
        dst_port: be16(frame, tcp + 2),
        seq: be32(frame, tcp + 4),
        ack: be32(frame, tcp + 8),
        has_ack: frame[tcp + 13] & 0x10 != 0,
        payload_offset: payload,
        payload_length: ip_end.saturating_sub(payload),
    })
}

/// Continuous RTT of the game connection: time from our data segment to the server ACK
/// that covers it (both timestamps from the same driver). Retransmissions are skipped (Karn's algorithm).
#[derive(Default)]
pub struct TcpRttTracker {
    in_flight: VecDeque<(u32, f64)>, // (seq end, sent ms)
    highest_sent: u32,
    highest_received: u32,
    sent_started: bool,
    received_started: bool,
}

impl TcpRttTracker {
    /// Returns true if this is a retransmission (our data was lost).
    pub fn on_outbound(&mut self, s: &TcpSegment, at_ms: f64) -> bool {
        if s.payload_length == 0 {
            return false;
        }
        let end = s.seq.wrapping_add(s.payload_length as u32);
        if self.sent_started && end.wrapping_sub(self.highest_sent) as i32 <= 0 {
            self.in_flight.clear(); // retransmission — this data no longer gives an honest sample
            return s.payload_length > 1; // 0–1 bytes is a keep-alive, not a loss
        }
        self.sent_started = true;
        self.highest_sent = end;
        if self.in_flight.len() < 512 {
            self.in_flight.push_back((end, at_ms));
        }
        false
    }

    /// Returns true if the server re-sent data we already had (its packet or our ACK was lost).
    pub fn is_inbound_retransmit(&mut self, s: &TcpSegment) -> bool {
        if s.payload_length <= 1 {
            return false;
        }
        let end = s.seq.wrapping_add(s.payload_length as u32);
        if self.received_started && end.wrapping_sub(self.highest_received) as i32 <= 0 {
            return true;
        }
        self.received_started = true;
        self.highest_received = end;
        false
    }

    /// RTT in ms, or None if this ACK acknowledged nothing new.
    pub fn on_inbound(&mut self, s: &TcpSegment, at_ms: f64) -> Option<f64> {
        if !s.has_ack {
            return None;
        }
        let mut sent = None;
        while let Some(&(seq_end, sent_ms)) = self.in_flight.front() {
            if (s.ack.wrapping_sub(seq_end) as i32) < 0 {
                break;
            }
            self.in_flight.pop_front();
            sent = Some(sent_ms); // newest acknowledged segment — least affected by delayed ACK
        }
        sent.map(|t| at_ms - t)
    }
}

/// The last few timestamps, so TCP retransmissions are not counted twice.
pub struct RecentSet {
    ring: Vec<i64>,
    set: HashSet<i64>,
    next: usize,
}

impl RecentSet {
    pub fn new(capacity: usize) -> Self {
        RecentSet { ring: vec![0; capacity], set: HashSet::with_capacity(capacity + 1), next: 0 }
    }

    pub fn add(&mut self, value: i64) -> bool {
        if !self.set.insert(value) {
            return false;
        }
        if self.set.len() > self.ring.len() {
            self.set.remove(&self.ring[self.next]);
        }
        self.ring[self.next] = value;
        self.next = (self.next + 1) % self.ring.len();
        true
    }
}

/// Converts an Npcap timestamp to the QPC timescale.
/// The offset between them = min(QPC_received − Npcap_timestamp) over the last 5 seconds:
/// the minimum corresponds to packets delivered without delay, so the driver timestamp
/// precision is kept, and the sliding window prevents drift or a stale offset from building up.
pub struct ClockCalibrator {
    min: [f64; Self::BUCKETS],
    second: [i64; Self::BUCKETS],
}

impl ClockCalibrator {
    const BUCKETS: usize = 5;

    pub fn new() -> Self {
        ClockCalibrator { min: [0.0; Self::BUCKETS], second: [-1000; Self::BUCKETS] }
    }

    pub fn qpc_ms_of(&mut self, npcap_ms: f64, received_qpc_ms: f64) -> f64 {
        let delta = received_qpc_ms - npcap_ms;
        let sec = (received_qpc_ms / 1000.0) as i64;
        let i = sec.rem_euclid(Self::BUCKETS as i64) as usize;
        if self.second[i] != sec {
            self.second[i] = sec;
            self.min[i] = delta;
        } else if delta < self.min[i] {
            self.min[i] = delta;
        }

        let mut offset = f64::MAX;
        for k in 0..Self::BUCKETS {
            if sec - self.second[k] < Self::BUCKETS as i64 && self.min[k] < offset {
                offset = self.min[k];
            }
        }
        npcap_ms + offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4_tcp_frame(payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 14];
        f[12] = 0x08; // IPv4
        let total = 20 + 20 + payload.len();
        let mut ip = vec![0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0, 0, 64, 6, 0, 0, 10, 0, 0, 1, 203, 0, 113, 42];
        f.append(&mut ip);
        let mut tcp = vec![0x2E, 0xE0, 0xC0, 0x00, 0, 0, 0, 100, 0, 0, 0, 200, 0x50, 0x10, 0, 0, 0, 0, 0, 0];
        f.append(&mut tcp);
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn parses_ethernet_ipv4() {
        let f = ipv4_tcp_frame(&[1, 2, 3]);
        let s = parse_tcp(&f, 1).unwrap();
        assert_eq!(crate::util::ip_text(s.src_ip), "10.0.0.1");
        assert_eq!(crate::util::ip_text(s.dst_ip), "203.0.113.42");
        assert_eq!((s.src_port, s.dst_port), (12000, 49152));
        assert_eq!((s.seq, s.ack, s.has_ack), (100, 200, true));
        assert_eq!((s.payload_offset, s.payload_length), (54, 3));
    }

    #[test]
    fn finds_ping_reply() {
        let sent = 5_000.0;
        let mut payload = vec![9, 9];
        payload.extend_from_slice(&SIGNATURE);
        payload.extend_from_slice(&(CLIENT_CLOCK_BASE + 5_000).to_le_bytes());
        let mut seen = RecentSet::new(4);
        let mut got = vec![];
        assert!(scan(&payload, sent + 42.0, &mut seen, |s, r| got.push((s, r))));
        assert_eq!(got, vec![(5_000.0, 42.0)]);
        // the same reply again (retransmission) is found but not reported twice
        assert!(scan(&payload, sent + 50.0, &mut seen, |s, r| got.push((s, r))));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn tracks_tcp_rtt() {
        let mut t = TcpRttTracker::default();
        let out = TcpSegment { seq: 1000, payload_length: 10, ..Default::default() };
        assert!(!t.on_outbound(&out, 100.0));
        let ack = TcpSegment { ack: 1010, has_ack: true, ..Default::default() };
        assert_eq!(t.on_inbound(&ack, 130.0), Some(30.0));
        assert!(t.on_outbound(&out, 200.0)); // retransmission
    }

    #[test]
    fn recent_set_forgets_oldest() {
        let mut r = RecentSet::new(2);
        assert!(r.add(1) && r.add(2) && r.add(3));
        assert!(r.add(1)); // 1 was evicted
        assert!(!r.add(3));
    }
}
