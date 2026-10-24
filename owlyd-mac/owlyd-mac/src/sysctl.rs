//! sysctl(3) bindings, and the itlawdl kext's knobs on top of them.
//!
//! The kext (branches awdl-v2 of this repository) exposes:
//!   debug.itlawdl        int rw   0 stop, 1 listen, 2 cell
//!   debug.itlawdl_cell   int rw   pin fw mac type (0 auto = PIBSS)
//!   debug.itlawdl_stats  string   counters
//!   debug.itlawdl_rx     opaque   drains received frames
//!   debug.itlawdl_tx     opaque   injects a frame
//!
//! Their wire formats are the contract with tools/itlawdlctl.c, which
//! spells them out.

use std::ffi::CString;
use std::io;

#[allow(unsafe_code)]
unsafe extern "C" {
    fn sysctlbyname(
        name: *const libc::c_char,
        oldp: *mut libc::c_void,
        oldlenp: *mut libc::size_t,
        newp: *const libc::c_void,
        newlen: libc::size_t,
    ) -> libc::c_int;
}

fn cstr(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::other("a NUL in the sysctl name"))
}

/// Reads an int sysctl.
#[allow(unsafe_code)]
pub fn get_int(name: &str) -> io::Result<i32> {
    let cname = cstr(name)?;
    let mut value: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    let r = unsafe {
        sysctlbyname(
            cname.as_ptr(),
            (&mut value as *mut i32).cast(),
            (&mut len as *mut usize).cast(),
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

/// Writes an int sysctl.
#[allow(unsafe_code)]
pub fn set_int(name: &str, value: i32) -> io::Result<()> {
    let cname = cstr(name)?;
    let r = unsafe {
        sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&value as *const i32).cast::<libc::c_void>(),
            std::mem::size_of::<i32>(),
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads an opaque sysctl into `buf`; returns how many bytes it held.
/// A buffer too small gets a truncated front, which the RX record parser
/// tolerates by stopping at the record that doesn't fit.
#[allow(unsafe_code)]
pub fn read_into(name: &str, buf: &mut [u8]) -> io::Result<usize> {
    let cname = cstr(name)?;
    let mut len = buf.len();
    let r = unsafe {
        sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            (&mut len as *mut usize).cast(),
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(len.min(buf.len()))
}

/// Writes an opaque sysctl (the injection request).
#[allow(unsafe_code)]
pub fn write_all(name: &str, data: &[u8]) -> io::Result<()> {
    let cname = cstr(name)?;
    let r = unsafe {
        sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            data.as_ptr().cast::<libc::c_void>(),
            data.len(),
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads a string sysctl (the stats block), trimmed.
pub fn read_string(name: &str) -> io::Result<String> {
    let mut buf = vec![0u8; 1024];
    let n = read_into(name, &mut buf)?;
    let n = buf[..n]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(n.min(buf.len()));
    Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
}

/// The kext's rx counter since the session came up, off its stats' first
/// line. The ramp watches it to tell a radio that hears from one the
/// first frames made deaf.
pub fn radio_rx() -> Option<u64> {
    let s = read_string("debug.itlawdl_stats").ok()?;
    s.lines()
        .next()?
        .strip_prefix("rx=")?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// The kext's inject counters, off its stats: `ok` writes taken, `err`
/// refused, `done` TX completions seen for the inject queue (v2.5+),
/// `done_err` of them FAILED (v2.6+). Fields the running kext is too
/// old to say read as 0 — a v2.4 kext reports `done=0` forever, which
/// the ramp must not read as "never completed".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InjectStats {
    pub ok: u64,
    pub err: u64,
    pub done: u64,
    pub done_err: u64,
}

/// The kext's inject counters, off its stats.
pub fn radio_inject() -> Option<InjectStats> {
    let s = read_string("debug.itlawdl_stats").ok()?;
    let l = s.lines().find(|l| l.starts_with("inject"))?;
    let mut st = InjectStats::default();
    for w in l.split_whitespace() {
        if let Some((k, v)) = w.split_once('=') {
            if let Ok(v) = v.parse::<u64>() {
                match k {
                    "ok" => st.ok = v,
                    "err" => st.err = v,
                    "done" => st.done = v,
                    "done_err" => st.done_err = v,
                    _ => {}
                }
            }
        }
    }
    Some(st)
}

/// The kext's firmware-error count (v2.6+): every dump iwx_nic_error
/// printed — the wedge's signature. `None` when the running kext is
/// older and does not say (the ramp then judges on rx alone).
pub fn radio_fw_err() -> Option<u64> {
    let s = read_string("debug.itlawdl_stats").ok()?;
    for l in s.lines() {
        for w in l.split_whitespace() {
            if let Some(("fw_err", v)) = w.split_once('=') {
                if let Ok(v) = v.parse::<u64>() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// A firmware error dump's numbers, off the kext's stats (v2.7.2+
/// kexts): the session's FIRST dump — the assert that killed it, not
/// the corpse's later ones. The LMAC half (fwid=, il1=, hcmd=) and the
/// UMAC half (umac id=/il=/d1=/d2=/d3=/ch=): the UMAC owns the
/// stations and queues, its data1-3 name what it choked on. A kext
/// too old to say them reads back as all zeros; `None` when the stats
/// carry no dump identity at all.
#[derive(Debug, Default, Clone, Copy)]
pub struct FwDump {
    pub fwid: u32,
    pub il1: u32,
    pub hcmd: u32,
    pub u_id: u32,
    pub u_il1: u32,
    pub u_d1: u32,
    pub u_d2: u32,
    pub u_d3: u32,
    pub u_ch: u32,
}

fn hex_u32(v: &str) -> Option<u32> {
    let h = v
        .strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .unwrap_or(v);
    u32::from_str_radix(h, 16).ok()
}

/// The last error dump's identity, as the stats name it.
pub fn radio_fw_dump() -> Option<FwDump> {
    let s = read_string("debug.itlawdl_stats").ok()?;
    let mut d = FwDump::default();
    let mut any = false;
    for l in s.lines() {
        for w in l.split_whitespace() {
            if let Some((k, v)) = w.split_once('=') {
                if let Some(n) = hex_u32(v) {
                    match k {
                        "fwid" => {
                            d.fwid = n;
                            any = true;
                        }
                        "il1" => d.il1 = n,
                        "hcmd" => d.hcmd = n,
                        // the umac line: id=/il=/d1=/d2=/d3=/ch=
                        "id" => {
                            d.u_id = n;
                            any = true;
                        }
                        "il" => d.u_il1 = n,
                        "d1" => d.u_d1 = n,
                        "d2" => d.u_d2 = n,
                        "d3" => d.u_d3 = n,
                        "ch" => d.u_ch = n,
                        _ => {}
                    }
                }
            }
        }
    }
    any.then_some(d)
}

/// The session sysctl's states, as the kext reads them back.
pub mod state {
    pub const OFF: i32 = 0;
    pub const CELL: i32 = 2;
    /// Refused: the Wi-Fi interface is still up.
    pub const BUSY_WIFI_ON: i32 = -1;
    pub const NO_CARD: i32 = -2;
    pub const FAILED: i32 = -3;

    /// What a state means, in the daemon's words.
    pub fn explain(v: i32) -> String {
        match v {
            BUSY_WIFI_ON => "le Wi-Fi est encore allumé (coupe-le, ou laisse le démon le faire)"
                .into(),
            NO_CARD => "aucune carte gérée par le kext".into(),
            FAILED => "refus du firmware (carte jamais alimentée ? allume le Wi-Fi une fois, \
                       coupe-le, puis relance)"
                .into(),
            _ => format!("état {v}"),
        }
    }
}
