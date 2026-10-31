// AionPingMeter — a lightweight ping meter for Aion 2 (Global).
//
// How ping is measured:
//   The game client sends the server a ping carrying its own timestamp = QPC in ms + 16 777 216 000,
//   and the server echoes it back in a packet with opcode 0x0336. We capture that reply via Npcap
//   and compute   ping = QPC_at_packet_arrival − QPC_from_packet.
//   Both values come from the same hardware timer (QueryPerformanceCounter); the Windows
//   system clock is never used, so clock corrections cannot introduce an offset.
//
// The program only reads AION2.exe traffic and never sends anything anywhere.

#![windows_subsystem = "windows"]

mod capture;
mod overlay;
mod parser;
mod pcap;
mod probe;
mod render;
mod settings;
mod stats;
mod util;

use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds = |default: u32| args.get(1).and_then(|s| s.parse().ok()).unwrap_or(default);
    match args.first().map(String::as_str) {
        Some("--render-demo") if args.len() > 1 => return probe::render_demo(&args[1]),
        Some("--gaps") => return probe::gaps(seconds(60)),
        Some("--probe") => return probe::run(seconds(20)),
        _ => {}
    }

    // One instance only.
    let name = util::wide("AionPingMeter.Aion2.SingleInstance");
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if mutex.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return 0;
    }

    let _gdip = render::GdiPlus::start();
    let stats = Arc::new(stats::PingStats::new(false));
    let captures = capture::CaptureManager::start(stats.clone());
    overlay::run(stats);
    drop(captures);
    unsafe { CloseHandle(mutex) };
    0
}
