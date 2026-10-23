//! owlyd-mac: the AWDL engine — owly-awdl's node — on the itlawdl kext's
//! cell, with the AirDrop receiver on top.
//!
//! It brings the PIBSS cell up (the kext of this repository, v2.4),
//! runs the node between the kext's sysctls (the air) and a utun
//! interface (the host's IPv6), and prints what happens: peers that
//! turn valid, the kext's counters every 10 s. Since v0.1.5 the
//! AirDrop end rides along (the `airdrop` module): `_airdrop._tcp`
//! announced over mDNS in bursts, the TLS receiver on 8770 an iPhone
//! requires before it lists anyone, the prompts when one sends.
//! Ctrl-C stops it and puts everything back.
//!
//! The AWDL address is the station's own (en0's): the kext's cell
//! context carries that address as its node address, and it is the one
//! the firmware ACKs unicast for — where owly on Linux derives a
//! locally administered one, because there the monitor can't carry the
//! station's. The two setups meet the same requirement from opposite
//! sides.

mod airdrop;
mod air;
mod netif;
mod sysctl;

use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use air::{Gate, KextAir};
use netif::UtunSetup;
use owly_airdrop::driver::receiver::ReceiverEvent;
use owly_airdrop::proto::messages::Offer;
use owly_awdl::{Channel, Config, Ipv6Packet, MacAddr, PeerInfo, Presence, RadioConstraints};
use sysctl::state;

use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, watch};

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
    println!("== owlyd-mac v{}", env!("CARGO_PKG_VERSION"));
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
    // put it back after — on the way out AND on any failure, so a
    // startup error never leaves the Mac without its Wi-Fi.
    let mut wifi = Wifi::new(STATION);
    wifi.off()?;

    if let Err(e) = session(addr, &name, &mut wifi) {
        session_down_checked();
        wifi.on();
        return Err(e);
    }
    Ok(())
}

/// Everything that runs between the interface going down and coming
/// back: the cell, the utun, the engine, the monitor. The caller puts
/// the interface back if any of it fails.
fn session(addr: MacAddr, name: &str, wifi: &mut Wifi) -> io::Result<()> {
    bring_cell_up(wifi)?;
    let utun = UtunSetup::open(addr)?;
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
    // The AirDrop end binds on the utun's index and the session's
    // link-local, and the engine's open() takes the utun with it — both
    // are said here, before the move. The loop-in end — the daemon's
    // direct lane into the engine's netif queue — comes back out of the
    // engine's own open(), so it waits in a box the closure fills.
    let link_local = addr.link_local();
    let scope = utun.index;
    // The ramp the engine's every frame out must pass (v0.1.6: the v06
    // run froze rx at the engine's first frames; a staged climb names
    // the killer — one frame or the pace).
    let gate = Arc::new(Gate::new());
    let gate_engine = gate.clone();
    let loop_in: Arc<std::sync::Mutex<Option<mpsc::Sender<Ipv6Packet>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let loop_in_engine = loop_in.clone();
    let running = owly_awdl::driver::spawn(
        cfg,
        move || {
            let (netif, host) = utun.register()?;
            *loop_in_engine.lock().unwrap() = Some(host);
            Ok((KextAir::new(gate_engine)?, netif))
        },
        peers_tx,
    )
    .map_err(|e| io::Error::other(format!("le moteur AWDL n'a pas démarré : {e}")))?;
    let host = loop_in
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| io::Error::other("le moteur n'a pas rendu l'entrée utun"))?;
    println!("== moteur AWDL lancé (canal {CHANNEL}, présence fixe, rampe 1→4→12 tr/s puis libre). Ctrl-C pour arrêter.\n");

    let failed = running.failed();
    let engine = running.progress.clone();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        // Phase 2: the receiver and its announcements. A failure here (the
        // port taken, /var/db unwritable) must not take the engine down
        // with it — the session stays a diagnostic one.
        let airdrop = match airdrop::start(scope, link_local, name, host).await {
            Ok(a) => Some(a),
            Err(e) => {
                println!("== AirDrop n'a pas pu démarrer : {e} — le moteur continue sans lui");
                None
            }
        };
        monitor(peers_rx, failed, engine, gate, airdrop.map(|a| a.events)).await
    });
    println!("\n== arrêt…");
    running.stop();
    // Give the kext's reader thread a beat to see the channel close and
    // leave the sysctl alone before the session -- and its ring with it
    // -- goes away.
    std::thread::sleep(Duration::from_millis(50));
    session_down_checked();
    wifi.on();
    println!("== session arrêtée, Wi-Fi rendu. À la prochaine.");
    Ok(())
}

/// The watcher loop: peers, failures, counters, the AirDrop receiver's
/// events, Ctrl-C. The engine's iteration count rides along every 10 s: a
/// count that stands still is a blocked engine, one that runs away is a
/// spin — the v03 field run showed neither, because nothing told us which
/// it was.
async fn monitor(
    mut peers: watch::Receiver<Vec<PeerInfo>>,
    mut failed: watch::Receiver<Option<String>>,
    engine: Arc<AtomicU64>,
    gate: Arc<Gate>,
    airdrop: Option<mpsc::Receiver<ReceiverEvent>>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    // The receiver's end, when it started; once its channel closes the
    // branch is disabled by the flag (a closed channel resolves at once,
    // which would busy-loop the monitor).
    let mut events = airdrop;
    let mut airdrop_alive = events.is_some();
    let mut last_peers: Vec<PeerInfo> = Vec::new();
    let mut last_iters: u64 = 0;
    let mut inject: Option<(u64, u64)> = None;
    let mut warned_inject = false;
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
                    // A kext before v2.4 never frees the inject queue's
                    // slots: ok freezes (330 on the AX211) and err climbs
                    // by every frame the engine builds. Say it once, in
                    // the user's words, instead of a silent dead radio.
                    let now = sysctl::radio_inject();
                    if let (Some((ok, err)), Some((was_ok, was_err))) = (now, inject) {
                        if !warned_inject && err > was_err + 100 && ok == was_ok {
                            println!("— kext : la file d'émission sature (ok={ok}, err={err}) — c'est la limite des ~330 trames des kexts v2.2/v2.3 ; le kext v2.4 la recycle");
                            warned_inject = true;
                        }
                    }
                    inject = now;
                }
                let iters = engine.load(Ordering::Relaxed);
                println!("— moteur : {} itérations en 10 s", iters - last_iters);
                println!("— {}", gate.report());
                last_iters = iters;
            }
            event = async {
                match events.as_mut() {
                    Some(events) => events.recv().await,
                    None => std::future::pending().await,
                }
            }, if airdrop_alive => {
                let Some(event) = event else {
                    airdrop_alive = false; // the receiver is gone
                    continue;
                };
                match event {
                    ReceiverEvent::Discovered { peer, .. } => {
                        println!(
                            "— AirDrop : vu par [{}] — le /Discover a passé, le nom devrait s'afficher",
                            peer.ip()
                        );
                    }
                    ReceiverEvent::Request(request) => {
                        let ask = &request.ask;
                        println!(
                            "— AirDrop : {:?} ({:?}) veut envoyer :",
                            ask.sender_computer_name, ask.sender_model_name
                        );
                        match &ask.offer {
                            Offer::Files(files) => {
                                for f in files {
                                    println!(
                                        "   {:?} {} {}",
                                        f.name,
                                        f.file_type,
                                        f.size.map(|s| format!("{s} o")).unwrap_or_default()
                                    );
                                }
                            }
                            Offer::Urls(urls) => {
                                for u in urls {
                                    println!("   {u:?}");
                                }
                            }
                        }
                        print!("— accepter ? [o/N] ");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                        // Ctrl-C while the prompt is up must still stop the
                        // daemon, not just decline the transfer.
                        let yes = tokio::select! {
                            line = stdin.next_line() => {
                                matches!(line, Ok(Some(l))
                                    if l.trim().eq_ignore_ascii_case("o")
                                        || l.trim().eq_ignore_ascii_case("oui"))
                            }
                            _ = tokio::signal::ctrl_c() => false,
                        };
                        if yes {
                            request.accept();
                            println!("— accepté, réception en cours…");
                        } else {
                            request.decline();
                            println!("— refusé");
                        }
                    }
                    ReceiverEvent::Withdrawn { .. } => {
                        println!("— AirDrop : l'envoi a été annulé de l'autre côté");
                    }
                    ReceiverEvent::Urls { urls, .. } => {
                        println!("— AirDrop : lien reçu :");
                        for u in &urls {
                            println!("   {u:?}");
                        }
                    }
                    ReceiverEvent::Received { paths, .. } => {
                        println!("— AirDrop : {} fichier(s) reçu(s) :", paths.len());
                        for p in &paths {
                            println!("   {}", p.display());
                        }
                        // They landed as root's; sudo says whose they are.
                        airdrop::give_back_to_user(&paths);
                    }
                    ReceiverEvent::Failed { reason, .. } => {
                        println!("— AirDrop : échec du transfert : {reason}");
                    }
                }
            }
        }
    }
}

/// Brings the cell up and PROVES it can hear: a session the firmware
/// brought up half-way looks perfectly healthy to the sysctls (v0.1.3's
/// field run: state=2, injections accepted, 90 s of rx=0 — not even the
/// infra beacons), so the only honest check is to listen. Twelve seconds
/// of anything — beacons, a peer's action frames — means the radio
/// lives; nothing at all means the session is deaf, and the cure that
/// has worked every time so far is a full power cycle (Wi-Fi on → off)
/// right before the cell goes up: that is the sequence every session
/// that ever received was brought up with.
fn bring_cell_up(wifi: &mut Wifi) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        session_up()?;
        println!("== j'écoute la radio 12 s — elle doit capter quelque chose (compteur toutes les 2 s)…");
        // v0.1.5: the v05 field run passed the "51 frames" check and then
        // received nothing for 100 s, and the total alone cannot say when
        // the radio died: before the engine started (a cell the firmware
        // brought up half-way, the kext's to fix) or exactly as the first
        // frames went out (our own TX choking its receive). The counter,
        // every 2 s, can.
        let mut samples = [0u64; 6];
        for slot in &mut samples {
            std::thread::sleep(Duration::from_secs(2));
            *slot = sysctl::radio_rx().unwrap_or(0);
        }
        let report = samples
            .iter()
            .enumerate()
            .map(|(i, n)| format!("+{}s={}", (i + 1) * 2, n))
            .collect::<Vec<_>>()
            .join(" ");
        println!("   rx : {report}");
        let total = samples[samples.len() - 1];
        if total > 0 {
            // The last three samples identical means nothing arrived in
            // the final 4 s of listening: the cell heard once and then
            // died. Say it and carry on — the engine's own stats are the
            // second half of the diagnosis.
            if samples[samples.len() - 3..].iter().all(|n| *n == total) {
                println!(
                    "== la réception s'est figée pendant l'écoute (rx bloqué à {total}) — cellule à moitié morte ; le moteur démarre quand même, surveille rx"
                );
            }
            println!("== la radio reçoit : {total} trames. En avant.");
            return Ok(());
        }
        attempt += 1;
        if attempt >= 3 {
            return Err(io::Error::other(
                "la radio reste muette (rx=0) après 3 essais. Envoie le journal du kext : \
                 sudo log show --last 15m --predicate 'eventMessage contains \"itlawdl\"'",
            ));
        }
        println!(
            "== la radio ne capte rien (essai {attempt}/2) — je la recycle : session basse, Wi-Fi on → off, session haute…"
        );
        session_down_checked();
        wifi.cycle();
    }
}

/// Brings the cell up, with the kext's own words for a refusal.
fn session_up() -> io::Result<()> {
    // A session left up by a run that died badly (a killed terminal, a
    // SIGKILL) must come down -- and be SEEN down -- before a new one
    // goes up. The kext applies the writes asynchronously (a
    // thread_call reads `want` when it fires), so an OFF immediately
    // followed by a CELL can collapse into one CELL against a session
    // that is already up, which returns early and keeps the previous
    // ring and counters: the v03 field run began against a session
    // like that, and its reader drained a dead session's ring.
    if let Ok(st) = sysctl::get_int("debug.itlawdl") {
        if st != state::OFF {
            println!("== une session AWDL traîne d'un run précédent (état {st}) — je la démonte…");
            sysctl::set_int("debug.itlawdl", state::OFF)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match sysctl::get_int("debug.itlawdl") {
                    Ok(v) if v == state::OFF => break,
                    Ok(v) if v < 0 => {
                        return Err(io::Error::other(format!(
                            "la session zombie a refusé de descendre : {}",
                            state::explain(v)
                        )));
                    }
                    _ if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100))
                    }
                    _ => {
                        // One more word, then give up loudly: a zombie we
                        // cannot kill leaves the radio unusable for us.
                        sysctl::set_int("debug.itlawdl", state::OFF).ok();
                        std::thread::sleep(Duration::from_millis(500));
                        if !matches!(sysctl::get_int("debug.itlawdl"), Ok(v) if v == state::OFF) {
                            return Err(io::Error::other(
                                "la session zombie ne descend pas — relance, et si ça persiste, redémarre",
                            ));
                        }
                        break;
                    }
                }
            }
        }
    }
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

/// Session down, and SEEN down: the kext applies the writes through a
/// thread_call that reads `want` when it fires, so a down followed
/// quickly by an up can collapse into one — leaving the previous
/// session (and its ring) in place under the new one's name. Every
/// teardown that precedes a new session up waits for the OFF here.
fn session_down_checked() {
    let _ = sysctl::set_int("debug.itlawdl", state::OFF);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(sysctl::get_int("debug.itlawdl"), Ok(v) if v == state::OFF) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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

    /// A full power cycle, for a radio that came up deaf: every session
    /// that ever received was brought up right after one of these (the
    /// daemon's own cut, or a manual one) — the ones brought up without
    /// it have all been mute. The final restore obligation is untouched:
    /// whatever the interface was when we started is what it gets back.
    fn cycle(&mut self) {
        println!("== Wi-Fi on (réveil radio)…");
        let _ = Command::new("/usr/sbin/networksetup")
            .args(["-setairportpower", &self.interface, "on"])
            .status();
        std::thread::sleep(Duration::from_secs(4));
        println!("== Wi-Fi off…");
        let _ = Command::new("/usr/sbin/networksetup")
            .args(["-setairportpower", &self.interface, "off"])
            .status();
        std::thread::sleep(Duration::from_secs(3));
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
