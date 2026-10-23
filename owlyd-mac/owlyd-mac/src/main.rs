//! owlyd-mac, phase 1: the AWDL engine — owly-awdl's node — on the
//! itlawdl kext's cell.
//!
//! It brings the PIBSS cell up (the kext of this repository, v2.2),
//! runs the node between the kext's sysctls (the air) and a utun
//! interface (the host's IPv6), and prints what happens: peers that
//! turn valid, the kext's counters every 10 s. Ctrl-C stops it and
//! puts everything back (session down, Wi-Fi back).
//!
//! The AirDrop records — mDNS, the picker, the receive flow — are
//! phase 2; this one proves the engine runs on the radio layer the
//! kext provides: sync with the iPhone, its mDNS packets arriving on
//! the utun, ours leaving.
//!
//! The AWDL address is the station's own (en0's): the kext's cell
//! context carries that address as its node address, and it is the one
//! the firmware ACKs unicast for — where owly on Linux derives a
//! locally administered one, because there the monitor can't carry the
//! station's. The two setups meet the same requirement from opposite
//! sides.

mod air;
mod netif;
mod sysctl;

use std::io;
use std::process::Command;
use std::time::Duration;

use air::KextAir;
use netif::UtunSetup;
use owly_awdl::{Channel, Config, MacAddr, PeerInfo, Presence, RadioConstraints};
use sysctl::state;

use tokio::sync::watch;

/// The channel the kext's cell sits on (owly's Intel channel, and the
/// AWDL social channel an iPhone always visits).
const CHANNEL: u8 = 6;
/// The Wi-Fi interface AirportItlwm drives.
const STATION: &str = "en0";

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(false)
        .init();
    if let Err(e) = run() {
        eprintln!("owlyd-mac : {e}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    if !euid_is_root() {
        return Err(io::Error::other("lance-le avec sudo (les sysctl du kext l'exigent)"));
    }
    let stats = sysctl::read_string("debug.itlawdl_stats")
        .map_err(|_| io::Error::other("le kext itlawdl n'est pas chargé (v2.2 ou plus)"))?;
    println!("== kext :\n{stats}");

    let addr = [STATION, "en1"]
        .iter()
        .find_map(|ifn| mac_of(ifn).ok())
        .ok_or_else(|| io::Error::other("aucune interface Wi-Fi (en0/en1) avec une MAC"))?;
    let name = hostname();
    println!("== station {STATION}, adresse AWDL {addr}, nom « {name} »");

    // The session needs the interface down; take it down ourselves and
    // put it back after, as the test scripts do.
    let mut wifi = Wifi::new(STATION);
    wifi.off()?;

    session_up()?;
    let utun = UtunSetup::open(addr).inspect_err(|_| session_down())?;
    println!("== utun {} (index {})", utun.name, utun.index);

    let cfg = {
        let mut cfg = Config::new(
            addr,
            &name,
            RadioConstraints {
                channel: Channel(CHANNEL),
                presence: Presence::Fixed,
            },
        );
        // The kext's ring and inject path take a single MSDU.
        cfg.max_frame = 2304;
        cfg
    };

    let (peers_tx, peers_rx) = watch::channel(Vec::<PeerInfo>::new());
    let running = owly_awdl::driver::spawn(cfg, move || Ok((KextAir::new(), utun.register()?)), peers_tx)
        .map_err(|e| {
            session_down();
            io::Error::other(format!("le moteur AWDL n'a pas démarré : {e}"))
        })?;
    println!("== moteur AWDL lancé (canal {CHANNEL}, présence fixe). Ctrl-C pour arrêter.\n");

    let failed = running.failed();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(monitor(peers_rx, failed));
    println!("\n== arrêt…");
    running.stop();
    session_down();
    wifi.on();
    println!("== session arrêtée, Wi-Fi rendu. À la prochaine.");
    Ok(())
}

/// The watcher loop: peers, failures, counters, Ctrl-C.
async fn monitor(
    mut peers: watch::Receiver<Vec<PeerInfo>>,
    mut failed: watch::Receiver<Option<String>>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_peers: Vec<PeerInfo> = Vec::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            r = peers.changed() => {
                if r.is_err() { return; }
                let now = peers.borrow_and_update().clone();
                for p in &now {
                    if !last_peers.iter().any(|q| q.mac == p.mac) {
                        println!(
                            "pair valide : {} « {} » devclass {} rssi {}",
                            p.mac,
                            p.name,
                            p.devclass,
                            match p.rssi { Some(r) => r.to_string(), None => "?".into() }
                        );
                    }
                }
                for q in &last_peers {
                    if !now.iter().any(|p| p.mac == q.mac) {
                        println!("pair parti  : {}", q.mac);
                    }
                }
                last_peers = now;
            }
            r = failed.changed() => {
                if r.is_err() { return; }
                if let Some(why) = failed.borrow_and_update().clone() {
                    eprintln!("le moteur s'est arrêté : {why}");
                    return;
                }
            }
            _ = ticker.tick() => {
                if let Ok(stats) = sysctl::read_string("debug.itlawdl_stats") {
                    let one = stats.lines().map(|l| l.trim()).collect::<Vec<_>>().join(" | ");
                    println!("— {one}");
                }
            }
        }
    }
}

/// Brings the cell up, with the kext's own words for a refusal.
fn session_up() -> io::Result<()> {
    sysctl::set_int("debug.itlawdl", state::OFF).ok(); // any stale session first
    sysctl::set_int("debug.itlawdl", state::CELL)?;
    for _ in 0..100 {
        match sysctl::get_int("debug.itlawdl") {
            Ok(v) if v == state::CELL => return Ok(()),
            Ok(v) if v < 0 => {
                return Err(io::Error::other(format!(
                    "la cellule n'a pas monté : {}",
                    state::explain(v)
                )));
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    Err(io::Error::other("la cellule n'a pas monté (délai)"))
}

fn session_down() {
    let _ = sysctl::set_int("debug.itlawdl", state::OFF);
}

/// The Wi-Fi interface's power, as networksetup sees it.
struct Wifi {
    interface: String,
    was_on: bool,
}

impl Wifi {
    fn new(interface: &str) -> Wifi {
        let was_on = Command::new("/usr/sbin/networksetup")
            .args(["-getairportpower", interface])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .is_some_and(|s| s.trim().ends_with("On"));
        Wifi {
            interface: interface.into(),
            was_on,
        }
    }

    fn off(&mut self) -> io::Result<()> {
        if !self.was_on {
            println!("== le Wi-Fi est déjà coupé");
            return Ok(());
        }
        println!("== coupure du Wi-Fi (le temps de la session)…");
        Command::new("/usr/sbin/networksetup")
            .args(["-setairportpower", &self.interface, "off"])
            .status()
            .map_err(|e| io::Error::other(format!("networksetup : {e}")))?;
        std::thread::sleep(Duration::from_secs(3));
        Ok(())
    }

    fn on(&mut self) {
        if !self.was_on {
            return;
        }
        let _ = Command::new("/usr/sbin/networksetup")
            .args(["-setairportpower", &self.interface, "on"])
            .status();
        self.was_on = false;
        println!("== Wi-Fi rallumé");
    }
}

/// The computer's name, as a Mac advertises it.
fn hostname() -> String {
    Command::new("/usr/sbin/scutil")
        .arg("--get")
        .arg("LocalHostName")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOST").ok())
        .unwrap_or_else(|| "Mac".into())
}

/// Darwin's `sockaddr_dl`, mirrored so this compiles anywhere libc's
/// headers don't reach.
#[repr(C)]
struct SockaddrDl {
    sdl_len: u8,
    sdl_family: u8,
    sdl_index: u16,
    sdl_type: u8,
    sdl_nlen: u8,
    sdl_alen: u8,
    sdl_slen: u8,
    sdl_data: [u8; 12],
}

/// AF_LINK, in Darwin's numbering.
const AF_LINK: u8 = 18;

/// An interface's MAC, off its AF_LINK address.
fn mac_of(interface: &str) -> io::Result<MacAddr> {
    #[allow(unsafe_code)]
    // SAFETY: getifaddrs hands a list freeifaddrs takes back whole.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut found = None;
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            let ifname = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            if ifname == interface
                && !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family as u32 == AF_LINK as u32
            {
                let sdl = &*(ifa.ifa_addr as *const SockaddrDl);
                if sdl.sdl_alen == 6 {
                    let base = sdl as *const SockaddrDl as *const u8;
                    let lla = base.add(
                        std::mem::offset_of!(SockaddrDl, sdl_data) + sdl.sdl_nlen as usize,
                    );
                    found = Some(MacAddr(std::array::from_fn(|i| *lla.add(i))));
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        found.ok_or_else(|| io::Error::other(format!("{interface}: pas d'adresse MAC")))
    }
}

fn euid_is_root() -> bool {
    #[allow(unsafe_code)]
    // SAFETY: geteuid takes nothing and returns a uid.
    unsafe { libc::geteuid() == 0 }
}
