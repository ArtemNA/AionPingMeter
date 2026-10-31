// Small helpers shared by the rest of the program: clocks, wide strings, IP formatting.

use std::sync::OnceLock;

use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetTickCount64};

/// Milliseconds since boot (monotonic, ~16 ms resolution) — the equivalent of Environment.TickCount64.
pub fn tick_ms() -> i64 {
    unsafe { GetTickCount64() as i64 }
}

/// QueryPerformanceCounter in milliseconds.
pub fn qpc_ms() -> f64 {
    static MS_PER_TICK: OnceLock<f64> = OnceLock::new();
    let k = *MS_PER_TICK.get_or_init(|| {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f) };
        1000.0 / f as f64
    });
    let mut t = 0i64;
    unsafe { QueryPerformanceCounter(&mut t) };
    t as f64 * k
}

/// Local time as "yyyy-MM-dd HH:mm:ss.fff".
pub fn local_timestamp() -> String {
    let mut t = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    let t: windows_sys::Win32::Foundation::SYSTEMTIME = t;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// An IPv4 address as stored on the wire / in Win32 tables, read as a little-endian u32
/// (so the first octet is the lowest byte).
pub type RawIp = u32;

pub fn ip_text(ip: RawIp) -> String {
    let [a, b, c, d] = ip.to_le_bytes();
    format!("{a}.{b}.{c}.{d}")
}

pub fn is_loopback(ip: RawIp) -> bool {
    ip.to_le_bytes()[0] == 127
}

/// UTF-16, zero-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copies a string into a fixed-size UTF-16 buffer (e.g. NOTIFYICONDATAW.szTip), truncating and zero-terminating.
pub fn copy_wide(dst: &mut [u16], s: &str) {
    let max = dst.len() - 1;
    let mut n = 0;
    for c in s.encode_utf16().take(max) {
        dst[n] = c;
        n += 1;
    }
    dst[n] = 0;
}

/// Rounds half away from zero and prints without decimals, like .NET's ToString("0").
pub fn f0(v: f64) -> String {
    format!("{}", v.round() as i64)
}
