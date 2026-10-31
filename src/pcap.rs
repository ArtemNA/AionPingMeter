// Npcap (wpcap.dll), loaded at run time so the program starts and explains itself even without Npcap.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW, SetDllDirectoryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::util::{RawIp, wide};

pub const TSTAMP_HOST_HIPREC_UNSYNCED: c_int = 5;

pub type Handle = *mut c_void;

/// struct pcap_pkthdr on Windows: timeval with 32-bit fields, then caplen and len.
#[repr(C)]
pub struct PktHeader {
    pub sec: u32,
    pub usec: u32,
    pub caplen: u32,
    pub len: u32,
}

#[repr(C)]
struct BpfProgram {
    len: u32,
    insns: *mut c_void,
}

#[repr(C)]
struct PcapIf {
    next: *mut PcapIf,
    name: *const c_char,
    description: *const c_char,
    addresses: *mut PcapAddr,
    flags: u32,
}

#[repr(C)]
struct PcapAddr {
    next: *mut PcapAddr,
    addr: *const u8, // struct sockaddr*
}

macro_rules! api {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        pub struct Api { $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)* }

        impl Api {
            unsafe fn load(dll: windows_sys::Win32::Foundation::HMODULE) -> Option<Api> {
                unsafe {
                    Some(Api { $($name: std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(GetProcAddress(dll, concat!(stringify!($name), "\0").as_ptr())?),)* })
                }
            }
        }
    };
}

api! {
    pcap_create: fn(*const c_char, *mut c_char) -> Handle;
    pcap_set_snaplen: fn(Handle, c_int) -> c_int;
    pcap_set_promisc: fn(Handle, c_int) -> c_int;
    pcap_set_timeout: fn(Handle, c_int) -> c_int;
    pcap_set_immediate_mode: fn(Handle, c_int) -> c_int;
    pcap_set_buffer_size: fn(Handle, c_int) -> c_int;
    pcap_set_tstamp_type: fn(Handle, c_int) -> c_int;
    pcap_activate: fn(Handle) -> c_int;
    pcap_datalink: fn(Handle) -> c_int;
    pcap_next_ex: fn(Handle, *mut *const PktHeader, *mut *const u8) -> c_int;
    pcap_close: fn(Handle);
    pcap_geterr: fn(Handle) -> *const c_char;
    pcap_compile: fn(Handle, *mut c_void, *const c_char, c_int, u32) -> c_int;
    pcap_setfilter: fn(Handle, *mut c_void) -> c_int;
    pcap_freecode: fn(*mut c_void);
    pcap_findalldevs: fn(*mut *mut c_void, *mut c_char) -> c_int;
    pcap_freealldevs: fn(*mut c_void);
}

/// The Npcap API, or None if Npcap is not installed.
pub fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        // Npcap installs wpcap.dll into System32\Npcap.
        let mut buf = [0u16; 260];
        let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
        if n > 0 && n < buf.len() {
            let dir = String::from_utf16_lossy(&buf[..n]) + "\\Npcap";
            if std::path::Path::new(&dir).is_dir() {
                SetDllDirectoryW(wide(&dir).as_ptr());
            }
        }
        let dll = LoadLibraryW(wide("wpcap.dll").as_ptr());
        if dll.is_null() { None } else { Api::load(dll) }
    })
    .as_ref()
}

fn api_or_err() -> Result<&'static Api, String> {
    api().ok_or_else(|| "Npcap not installed".to_string())
}

unsafe fn c_text(p: *const c_char) -> String {
    if p.is_null() { "?".into() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

/// An open Npcap capture; closed on drop.
pub struct Capture {
    api: &'static Api,
    p: Handle,
}

// The handle is used by one thread at a time.
unsafe impl Send for Capture {}

impl Capture {
    pub fn create(device: &str) -> Result<Capture, String> {
        let api = api_or_err()?;
        let mut err = [0 as c_char; 512];
        let name = CString::new(device).map_err(|e| e.to_string())?;
        let p = unsafe { (api.pcap_create)(name.as_ptr(), err.as_mut_ptr()) };
        if p.is_null() {
            return Err(unsafe { c_text(err.as_ptr()) });
        }
        Ok(Capture { api, p })
    }

    pub fn configure_and_activate(&self) -> Result<(), String> {
        let a = self.api;
        unsafe {
            (a.pcap_set_snaplen)(self.p, 65535);
            (a.pcap_set_promisc)(self.p, 0);
            (a.pcap_set_timeout)(self.p, 250);
            (a.pcap_set_immediate_mode)(self.p, 1); // no buffering — packets are delivered immediately
            (a.pcap_set_buffer_size)(self.p, 1 << 20);
            (a.pcap_set_tstamp_type)(self.p, TSTAMP_HOST_HIPREC_UNSYNCED); // QPC timestamps, if the driver supports them
            if (a.pcap_activate)(self.p) < 0 {
                return Err(self.error());
            }
        }
        Ok(())
    }

    pub fn datalink(&self) -> i32 {
        unsafe { (self.api.pcap_datalink)(self.p) }
    }

    pub fn error(&self) -> String {
        unsafe { c_text((self.api.pcap_geterr)(self.p)) }
    }

    pub fn set_filter(&self, filter: &str) -> bool {
        let Ok(f) = CString::new(filter) else { return false };
        let mut program = BpfProgram { len: 0, insns: std::ptr::null_mut() };
        let prog = &mut program as *mut BpfProgram as *mut c_void;
        unsafe {
            if (self.api.pcap_compile)(self.p, prog, f.as_ptr(), 1, 0xFFFF_FFFF) != 0 {
                return false;
            }
            let rc = (self.api.pcap_setfilter)(self.p, prog);
            (self.api.pcap_freecode)(prog);
            rc == 0
        }
    }

    /// 1 = packet, 0 = timeout, negative = error. On success the slices are valid until the next call.
    pub fn next(&self) -> (i32, Option<(&PktHeader, &[u8])>) {
        let mut h: *const PktHeader = std::ptr::null();
        let mut d: *const u8 = std::ptr::null();
        let rc = unsafe { (self.api.pcap_next_ex)(self.p, &mut h, &mut d) };
        if rc != 1 || h.is_null() || d.is_null() {
            return (rc, None);
        }
        let header = unsafe { &*h };
        let data = unsafe { std::slice::from_raw_parts(d, header.caplen as usize) };
        (rc, Some((header, data)))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe { (self.api.pcap_close)(self.p) };
    }
}

/// Npcap adapter names and their IPv4 addresses.
pub fn list_devices() -> Result<Vec<(String, Vec<RawIp>)>, String> {
    let api = api_or_err()?;
    let mut err = [0 as c_char; 512];
    let mut devs: *mut c_void = std::ptr::null_mut();
    if unsafe { (api.pcap_findalldevs)(&mut devs, err.as_mut_ptr()) } != 0 {
        return Err(format!("Npcap unavailable: {}", unsafe { c_text(err.as_ptr()) }));
    }
    let mut result = Vec::new();
    unsafe {
        let mut d = devs as *const PcapIf;
        while !d.is_null() {
            let name = if (*d).name.is_null() { String::new() } else { CStr::from_ptr((*d).name).to_string_lossy().into_owned() };
            let mut addrs = Vec::new();
            let mut a = (*d).addresses as *const PcapAddr;
            while !a.is_null() {
                let sa = (*a).addr;
                // sockaddr_in: family (u16), port, address
                if !sa.is_null() && u16::from_ne_bytes([*sa, *sa.add(1)]) == 2 {
                    addrs.push(u32::from_le_bytes([*sa.add(4), *sa.add(5), *sa.add(6), *sa.add(7)]));
                }
                a = (*a).next;
            }
            result.push((name, addrs));
            d = (*d).next;
        }
        (api.pcap_freealldevs)(devs);
    }
    Ok(result)
}
