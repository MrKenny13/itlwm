//! A utun interface as the driver's [`Netif`].
//!
//! utun is point-to-point: no Ethernet header, no MAC of its own, no
//! neighbour entries to keep (NET-2 is the kernel's to do over
//! link-local, and a point-to-point link needs no resolution). Packets
//! read and written carry a 4-byte address-family prefix, AF_INET6 (30,
//! in Darwin numbering) for ours.
//!
//! The interface comes up the classic way: a PF_SYSTEM socket bound to
//! the utun kernel control, `CTLIOCGINFO` for its ID, `connect` on a
//! free unit. It lasts as long as the descriptor. The address, MTU and
//! up state go through ifconfig, which root runs.
//!
//! v0.1.3: the reads no longer go through AsyncFd. The v03 field run
//! froze exactly as a cached-readiness spin would: the engine sent its
//! first MIF (before the loop), then nothing — no STATS line ever, not
//! one peer from a ring the reader kept feeding, not even the kernel's
//! own DAD packets read off the utun — with the thread alive at the
//! stop. AsyncFd's canonical pattern calls `clear_ready()` in the
//! WouldBlock arm; ours didn't, so one spurious readiness on the
//! kernel-control socket (kqueue reports these) would loop
//! `readable() → EAGAIN → readable()` forever — and inside the driver's
//! `biased` select, that one branch starves timers, peers, everything.
//! The utun now has a reader thread of its own, the pattern the kext
//! ring already uses and the field already proved: a non-blocking read,
//! a 2 ms pause when it comes back empty, packets handed to the engine
//! over a channel. The engine's end is a channel receive — a wedged fd
//! can no longer take the node down — and the reader says it is alive
//! every 10 s, as the ring's reader does. Writes stay inline and
//! non-blocking: EAGAIN drops the packet, as a full queue would.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use owly_awdl::driver::Netif;
use owly_awdl::{Ipv6Packet, MacAddr};

use tokio::sync::mpsc;

const AF_SYSTEM: libc::c_int = 32;
const SYSPROTO_CONTROL: libc::c_int = 2;
const AF_SYS_CONTROL: libc::c_ushort = 2;
/// `_IOWR('N', 3, struct ctl_info)`: 96 bytes of name, 4 of ID.
const CTLIOCGINFO: libc::c_ulong = 0xC064_4E03;
/// The utun kernel control, as XNU registers it (`bsd/net/if_utun.h`:
/// `UTUN_CONTROL_NAME`). The trailing `_control` is the name; the bare
/// "com.apple.net.utun" some pages show makes `CTLIOCGINFO` answer
/// ENOENT — the first release shipped that and died on real Macs.
const UTUN_CONTROL: &[u8] = b"com.apple.net.utun_control\0";
/// Darwin's AF_INET6, the utun prefix says.
const AF_INET6: u32 = 30;
/// `awdl0`'s MTU on Linux, and ours.
const MTU: u32 = 1450;
/// How long the reader waits between reads that came back empty.
const EMPTY_POLL: Duration = Duration::from_millis(2);
/// How often the reader says it is alive.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How many packets may wait for the engine at once.
const PACKETS: usize = 256;

#[repr(C)]
struct CtlInfo {
    ctl_id: libc::c_uint,
    ctl_name: [u8; 96],
}

#[repr(C)]
struct SockaddrCtl {
    sc_len: u8,
    sc_family: u8,
    ss_sysaddr: libc::c_ushort,
    sc_id: libc::c_uint,
    sc_unit: libc::c_uint,
    sc_reserved: [libc::c_uint; 5],
}

/// A utun opened and configured, ready to hand to the engine's thread.
pub struct UtunSetup {
    fd: OwnedFd,
    pub name: String,
    pub index: u32,
}

impl UtunSetup {
    /// Opens a free utun (units 10 and up: the system's own live below)
    /// and configures it: MTU 1450, the AWDL address's link-local, up.
    pub fn open(addr: MacAddr) -> io::Result<UtunSetup> {
        let (fd, unit) = open_control()?;
        let name = format!("utun{}", unit - 1);
        let cname = CString::new(name.as_str()).map_err(|_| io::Error::other("un nom de fou"))?;
        #[allow(unsafe_code)]
        // SAFETY: `cname` is a valid NUL-terminated string for the call.
        let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
        if index == 0 {
            return Err(io::Error::other(format!("{name}: pas d'index")));
        }

        // The address: modified EUI-64 from the AWDL address, as on any
        // host. DAD runs on it — utun gets no autoconf address of the
        // kernel's own, so ours is the link's one and only.
        let ll = format!("{}/64", addr.link_local());
        let ifs = "/sbin/ifconfig";
        for args in [
            vec![&name, "mtu", &MTU.to_string()],
            vec![&name, "inet6", &ll, "add"],
            vec![&name, "up"],
        ] {
            let out = Command::new(ifs)
                .args(&args)
                .output()
                .map_err(|e| io::Error::other(format!("{ifs} {}: {e}", args[0])))?;
            // An address already there is fine; anything else is not.
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if !out.status.success()
                && !said.contains("already exists")
                && !said.contains("existe déjà")
            {
                return Err(io::Error::other(format!(
                    "ifconfig {} {args:?}: {said}",
                    name
                )));
            }
        }
        tracing::info!("{name} up, {}, MTU {MTU}", addr.link_local());
        Ok(UtunSetup { fd, name, index })
    }

    /// Spawns the utun's reader thread; its channel is the engine's end,
    /// and a second copy of the channel's sending end comes back for the
    /// daemon itself — the loop-in lane. May be called from any thread —
    /// nothing here touches a runtime.
    ///
    /// v0.1.6: the loop-in lane exists because the v06 field run sent
    /// ~1700 mDNS announcements through the socket while the utun read
    /// "0 paquets lus au total" — macOS never routed one multicast
    /// packet into utun9, and each send's error was thrown away. The
    /// AirDrop end now pushes its announcements straight into this
    /// channel, as the kernel would have; the socket stays for the
    /// queries and answers, and its send errors are said out loud now.
    pub fn register(self) -> io::Result<(Utun, mpsc::Sender<Ipv6Packet>)> {
        let (tx, rx) = mpsc::channel(PACKETS);
        // The reader gets its own descriptor: the fd stays non-blocking
        // (writes must never stall the engine), so the reader simply
        // waits between empty reads, like the kext ring's reader does.
        let read_fd = dup_fd(&self.fd).map_err(|e| {
            io::Error::other(format!("{} : pas de second descripteur : {e}", self.name))
        })?;
        let name = self.name.clone();
        let reader_tx = tx.clone();
        thread::Builder::new()
            .name("utun-rx".into())
            .spawn(move || read_utun(read_fd, name, reader_tx))
            .map_err(|e| io::Error::other(format!("le fil de lecture utun : {e}")))?;
        Ok((
            Utun {
                fd: self.fd,
                rx,
                name: self.name,
                index: self.index,
                out: Vec::with_capacity(1500),
            },
            tx,
        ))
    }
}

/// The engine's end: a channel in, a non-blocking write out.
pub struct Utun {
    fd: OwnedFd,
    rx: mpsc::Receiver<Ipv6Packet>,
    pub name: String,
    /// Phase 2 binds its mDNS socket to the interface by this.
    #[allow(dead_code)]
    pub index: u32,
    out: Vec<u8>,
}

impl Netif for Utun {
    async fn recv(&mut self) -> io::Result<Ipv6Packet> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::other("le fil de lecture utun s'est arrêté"))
    }

    fn deliver(&mut self, packet: Ipv6Packet) {
        self.out.clear();
        self.out.extend_from_slice(&AF_INET6.to_ne_bytes());
        self.out.extend_from_slice(&packet.data);
        #[allow(unsafe_code)]
        // SAFETY: the buffer is `out.len()` long and outlives the call.
        let wrote = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                self.out.as_ptr().cast(),
                self.out.len(),
            )
        };
        if wrote < 0 {
            // EAGAIN drops the packet, as a full queue would.
            tracing::debug!("{} a refusé un paquet", self.name);
        }
    }
}

/// The utun reader thread's whole life: read the descriptor, keep the
/// IPv6 packets, hand them to the engine, say it is alive, and leave
/// when the engine is gone — or the descriptor under it.
fn read_utun(fd: OwnedFd, name: String, tx: mpsc::Sender<Ipv6Packet>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut packets: u64 = 0;
    let mut beat = Instant::now();
    tracing::info!("utun : lecteur démarré ({name})");
    loop {
        if tx.is_closed() {
            return; // the engine is gone
        }
        #[allow(unsafe_code)]
        // SAFETY: the buffer is `buf.len()` long and outlives the call.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) {
                // Nothing for now — same pace as the ring's reader.
                thread::sleep(EMPTY_POLL);
                if beat.elapsed() >= HEARTBEAT {
                    tracing::info!("utun : {packets} paquets lus au total");
                    beat = Instant::now();
                }
                continue;
            }
            tracing::warn!("utun : le descripteur a rendu {e} — j'arrête");
            return;
        }
        if n == 0 {
            tracing::warn!("utun : le descripteur est parti — j'arrête");
            return;
        }
        if n > 4 {
            let family = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
            if family == AF_INET6 {
                if let Some(packet) = Ipv6Packet::from_ip(buf[4..n as usize].to_vec()) {
                    if tx.blocking_send(packet).is_err() {
                        return; // the engine is gone
                    }
                    packets += 1;
                }
                // A malformed packet: dropped, as a full queue would.
            }
            // An AF_INET prefix or another family: not ours.
        }
        if beat.elapsed() >= HEARTBEAT {
            tracing::info!("utun : {packets} paquets lus au total");
            beat = Instant::now();
        }
    }
}

/// A second descriptor onto the same utun, for the reader thread.
#[allow(unsafe_code)]
fn dup_fd(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: `dup` takes a live fd and returns a new one (or -1).
    let d = unsafe { libc::dup(fd.as_raw_fd()) };
    if d < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `d` is a live descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(d) })
}

/// A utun control socket on a free unit, 10 and up.
#[allow(unsafe_code)]
fn open_control() -> io::Result<(OwnedFd, libc::c_uint)> {
    let fd = unsafe { libc::socket(AF_SYSTEM, libc::SOCK_DGRAM, SYSPROTO_CONTROL) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(unsafe_code)]
    // SAFETY: a socket that opened is owned from here on.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    nonblock_cloexec(fd.as_raw_fd())?;

    let mut info = CtlInfo {
        ctl_id: 0,
        ctl_name: [0; 96],
    };
    info.ctl_name[..UTUN_CONTROL.len()].copy_from_slice(UTUN_CONTROL);
    #[allow(unsafe_code)]
    // SAFETY: `info` is a valid ctl_info of the size the ioctl takes.
    if unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            CTLIOCGINFO as libc::c_ulong,
            (&mut info as *mut CtlInfo).cast::<libc::c_void>(),
        )
    } < 0
    {
        let e = io::Error::last_os_error();
        return Err(io::Error::other(format!(
            "le control utun du noyau est introuvable ({}) : {e}",
            String::from_utf8_lossy(&UTUN_CONTROL[..UTUN_CONTROL.len() - 1])
        )));
    }

    for unit in 10u32..40 {
        let addr = SockaddrCtl {
            sc_len: std::mem::size_of::<SockaddrCtl>() as u8,
            sc_family: AF_SYSTEM as u8,
            ss_sysaddr: AF_SYS_CONTROL,
            sc_id: info.ctl_id,
            sc_unit: unit,
            sc_reserved: [0; 5],
        };
        #[allow(unsafe_code)]
        // SAFETY: `addr` is a sockaddr_ctl of the length given.
        let r = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const SockaddrCtl).cast(),
                std::mem::size_of::<SockaddrCtl>() as u32,
            )
        };
        if r == 0 {
            return Ok((fd, unit));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EADDRINUSE) && e.raw_os_error() != Some(libc::EBUSY) {
            return Err(e);
        }
    }
    Err(io::Error::other("aucune unité utun libre"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CTLIOCGINFO` turns the control's name into its kernel ID; a
    /// name that isn't registered answers ENOENT. That is exactly how
    /// v0.1 died in the field (the bare `com.apple.net.utun`), so the
    /// name is now proven at test time, where the CI's macOS runner
    /// exercises it for real. Resolving needs no privileges — only
    /// connecting does.
    #[cfg(target_os = "macos")]
    #[test]
    fn utun_control_name_resolves() {
        #[allow(unsafe_code)]
        // SAFETY: an fd that opened is closed before the assert can end us.
        unsafe {
            let fd = libc::socket(AF_SYSTEM, libc::SOCK_DGRAM, SYSPROTO_CONTROL);
            assert!(fd >= 0, "socket: {}", io::Error::last_os_error());
            let mut info = CtlInfo {
                ctl_id: 0,
                ctl_name: [0; 96],
            };
            info.ctl_name[..UTUN_CONTROL.len()].copy_from_slice(UTUN_CONTROL);
            let r = libc::ioctl(
                fd,
                CTLIOCGINFO as libc::c_ulong,
                (&mut info as *mut CtlInfo).cast::<libc::c_void>(),
            );
            let said = io::Error::last_os_error();
            libc::close(fd);
            assert_eq!(r, 0, "CTLIOCGINFO a dit : {said}");
            assert_ne!(info.ctl_id, 0, "CTLIOCGINFO n'a pas donné d'ID");
        }
    }
}

#[allow(unsafe_code)]
fn nonblock_cloexec(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: the flags are read and written back through the same fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
        let fdflags = libc::fcntl(fd, libc::F_GETFD);
        if fdflags < 0 || libc::fcntl(fd, libc::F_SETFD, fdflags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
