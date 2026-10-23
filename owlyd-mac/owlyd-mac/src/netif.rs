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

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Command;

use owly_awdl::driver::Netif;
use owly_awdl::{Ipv6Packet, MacAddr};

use tokio::io::unix::AsyncFd;

const AF_SYSTEM: libc::c_int = 32;
const SYSPROTO_CONTROL: libc::c_int = 2;
const AF_SYS_CONTROL: libc::c_ushort = 2;
/// `_IOWR('N', 3, struct ctl_info)`: 96 bytes of name, 4 of ID.
const CTLIOCGINFO: libc::c_ulong = 0xC064_4E03;
/// Darwin's AF_INET6, the utun prefix says.
const AF_INET6: u32 = 30;
/// `awdl0`'s MTU on Linux, and ours.
const MTU: u32 = 1450;

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

/// A utun opened and configured, ready to register with a runtime.
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

    /// Registers the descriptor with the current runtime: the step that
    /// must run on the driver's thread.
    pub fn register(self) -> io::Result<Utun> {
        #[allow(unsafe_code)]
        // SAFETY: AsyncFd::register takes what it registers.
        let fd = unsafe { AsyncFd::register(self.fd) }.map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Utun {
            fd,
            name: self.name,
            index: self.index,
            buf: vec![0; 64 * 1024],
            out: Vec::with_capacity(1500),
        })
    }
}

/// The registered end: what the driver runs against.
pub struct Utun {
    fd: AsyncFd<OwnedFd>,
    pub name: String,
    /// Phase 2 binds its mDNS socket to the interface by this.
    #[allow(dead_code)]
    pub index: u32,
    buf: Vec<u8>,
    out: Vec<u8>,
}

impl Netif for Utun {
    async fn recv(&mut self) -> io::Result<Ipv6Packet> {
        loop {
            let mut guard = self.fd.readable().await?;
            #[allow(unsafe_code)]
            // SAFETY: the buffer is `buf.len()` long and outlives the call.
            let read = guard.try_io(|fd| {
                Ok(unsafe {
                    libc::read(
                        fd.as_raw_fd(),
                        self.buf.as_mut_ptr().cast(),
                        self.buf.len(),
                    )
                })
            });
            match read {
                Ok(Ok(n)) if n > 4 => {
                    let family = u32::from_ne_bytes(self.buf[0..4].try_into().unwrap());
                    if family != AF_INET6 {
                        continue; // an AF_INET prefix; not ours
                    }
                    if let Some(packet) = Ipv6Packet::from_ip(self.buf[4..n as usize].to_vec()) {
                        return Ok(packet);
                    }
                    // A malformed packet: dropped, as a full queue would.
                }
                Ok(Ok(0)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("{} est partie", self.name),
                    ));
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("{} a lâché : {e}", self.name),
                    ));
                }
                Err(_would_block) => {}
            }
        }
    }

    fn deliver(&mut self, packet: Ipv6Packet) {
        self.out.clear();
        self.out.extend_from_slice(&AF_INET6.to_ne_bytes());
        self.out.extend_from_slice(&packet.data);
        #[allow(unsafe_code)]
        // SAFETY: the buffer is `out.len()` long and outlives the call.
        let wrote = unsafe {
            libc::write(
                self.fd.get_ref().as_raw_fd(),
                self.out.as_ptr().cast(),
                self.out.len(),
            )
        };
        if wrote < 0 {
            // EAGAIN drops the packet, as a full queue would.
            tracing::debug!("{} a refusé un paquet", self.name);
        }
    }

    fn peer_up(&mut self, _mac: MacAddr) {}
    fn peer_down(&mut self, _mac: MacAddr) {}
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
    info.ctl_name[..b"com.apple.net.utun\0".len()].copy_from_slice(b"com.apple.net.utun\0");
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
        return Err(io::Error::last_os_error());
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
