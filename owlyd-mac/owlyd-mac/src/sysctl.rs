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
