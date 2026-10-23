//! Phase 2: the AirDrop receiver — what was missing for the Mac to be a
//! row in the share sheet at all.
//!
//! PSFs alone never put a device in the AirDrop list: an iPhone that hears
//! one only learns the AWDL peer exists. The list is DNS-SD over mDNS
//! (`_airdrop._tcp`, announced in bursts for as long as the session runs,
//! MDNS-7), and iOS lists a receiver only once its `/Discover` answers
//! `ReceiverComputerName` (DISC-1) — a TLS server on the announced port,
//! self-signed certificate and all (TLS-2, encryption only). All of that is
//! owly-airdrop's, validated on Linux against iOS 26; this module is the
//! macOS wiring.
//!
//! The mDNS packets leave through the utun like any host packet — the
//! engine reads them off the interface and carries them as AWDL data
//! frames — and the phone's arrive the same way, delivered into the utun
//! for the kernel to hand to the socket that joined `ff02::fb` on it. The
//! responder therefore needs no hook of its own in the engine: bind, join,
//! announce, answer, exactly as on Linux's `awdl0`.

use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::path::{Path, PathBuf};

use owly_airdrop::driver::mdns::{Mdns, Registration};
use owly_airdrop::driver::receiver::{Options, Receiver, ReceiverEvent};
use owly_airdrop::driver::tls::{rustls, Identity};
use owly_airdrop::proto::mdns::{service_id, Burst, Service, RECEIVER_FLAGS};
use owly_airdrop::proto::receiver::Config;

use tokio::sync::mpsc;

/// Apple's receivers listen here, and a sender without mDNS tries it
/// directly (DISC-5).
pub const AIRDROP_PORT: u16 = 8770;

/// Where the self-signed identity persists across runs, so the phone's
/// caches keep matching us between sessions.
const IDENTITY_DIR: &str = "/var/db/owlyd-mac";

/// owly-radio's `Announce::FIXED`, the burst a radio fixed to one channel
/// asks for (MDNS-7): the phone spends a slot or two in sixteen on our
/// channel, so a copy every 16 TU — one availability window — for a whole
/// sequence period (64), then a rest. A single announcement is easily
/// missed; a whole period of them is not.
const ANNOUNCE: Burst = Burst {
    count: 64,
    interval: std::time::Duration::from_micros(16_400),
    pause: std::time::Duration::from_secs(3),
};

/// The session's AirDrop end: a receiver on the utun's link-local, and the
/// mDNS registration that keeps announcing it. Dropping the registration
/// withdraws the records, so both live as long as the session does.
pub struct AirDrop {
    /// What the receiver wants asked or said; `monitor` drains it.
    pub events: mpsc::Receiver<ReceiverEvent>,
    _announce: Registration,
}

/// Brings the receiver up on the utun: the TLS listener on
/// `[link-local]:8770`, and the `_airdrop._tcp` records announced in
/// bursts. `scope` is the utun's interface index; `addr` its link-local
/// address; `name` what the share sheet will show (the computer name).
pub async fn start(scope: u32, addr: Ipv6Addr, name: &str) -> io::Result<AirDrop> {
    // The crypto provider must be in before any rustls builder; a second
    // install (tests, a second start) is not an error worth failing on.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // The identity persists, so the phone that saw us yesterday is not
    // shown a stranger today.
    let identity = Identity::load_or_generate(Path::new(IDENTITY_DIR))
        .map_err(|e| io::Error::other(format!("l'identité TLS ({IDENTITY_DIR}) : {e}")))?;

    let mut config = Config::new(name);
    config.model_name = "Mac".into();
    let destination = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let options = Options::new(config, destination);

    let bind = SocketAddrV6::new(addr, AIRDROP_PORT, 0, scope);
    let (receiver, events) = Receiver::bind(bind, &identity, options)
        .await
        .map_err(|e| io::Error::other(format!("le récepteur sur [{addr}%{scope}]:{AIRDROP_PORT} : {e}")))?;
    tokio::spawn(receiver.run());

    // A fresh service ID per session, as owly does (MDNS-1, MDNS-3).
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
    let id = service_id(random);
    let service = Service {
        instance: id.clone(),
        host: host_label(name),
        port: AIRDROP_PORT,
        addr,
        flags: RECEIVER_FLAGS,
    };
    let announce = Mdns::new(scope)
        .with_burst(Some(ANNOUNCE))
        .register(service)
        .await
        .map_err(|e| io::Error::other(format!("le répondeur mDNS sur l'interface {scope} : {e}")))?;

    println!(
        "== AirDrop : « {name} » (id {id}) annoncé sur [{addr}%{scope}]:{AIRDROP_PORT}, \
         drapeaux {RECEIVER_FLAGS:#05x} — il apparaîtra dans les feuilles de partage"
    );
    Ok(AirDrop {
        events,
        _announce: announce,
    })
}

/// A host label fit for `<label>.local`: mDNS host labels are the letters,
/// digits and hyphens of RFC 1123, and the share sheet's aesthetics like
/// them short.
pub fn host_label(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    if trimmed.is_empty() {
        "Mac".into()
    } else {
        trimmed.chars().take(63).collect()
    }
}

/// Files arrive owned by root (the daemon runs under sudo); give them back
/// to the user who launched it, when sudo says who that was.
pub fn give_back_to_user(paths: &[PathBuf]) {
    let (Some(uid), Some(gid)) = (sudo_uid(), sudo_gid()) else {
        return;
    };
    for path in paths {
        let _ = chown_tree(path, uid, gid);
    }
}

fn sudo_uid() -> Option<u32> {
    std::env::var("SUDO_UID").ok()?.trim().parse().ok()
}

fn sudo_gid() -> Option<u32> {
    std::env::var("SUDO_GID").ok()?.trim().parse().ok()
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    use std::os::unix::fs::chown;
    chown(path, Some(uid), Some(gid))?;
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use owly_airdrop::proto::mdns::{Message, Name, RData};

    fn service() -> Service {
        Service {
            instance: "0123456789ab".into(),
            host: "MacBook-de-Kenny".into(),
            port: AIRDROP_PORT,
            addr: "fe80::b0d9:efff:fe64:a87c".parse().unwrap(),
            flags: RECEIVER_FLAGS,
        }
    }

    /// MDNS-1 and MDNS-4: the four records, with the TXT flags the phone
    /// reads to know what we accept.
    #[test]
    fn the_records_are_what_a_phone_expects() {
        let records = service().records();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].name, Name::service());
        assert!(matches!(&records[0].data, RData::Ptr(target)
            if *target == Name::instance("0123456789ab")));
        let RData::Srv { port, .. } = &records[1].data else {
            panic!("SRV");
        };
        assert_eq!(*port, 8770);
        let RData::Txt(strings) = &records[2].data else {
            panic!("TXT");
        };
        assert_eq!(strings, &vec![format!("flags={RECEIVER_FLAGS}").into_bytes()]);
        let RData::Aaaa(addr) = &records[3].data else {
            panic!("AAAA");
        };
        assert_eq!(addr.octets(), service().addr.octets());
    }

    /// The announcement round-trips through the strict decoder, so what
    /// goes on the air is a DNS message a phone can read.
    #[test]
    fn the_announcement_decodes_back() {
        let bytes = service().announcement();
        assert!(bytes.len() > 150, "an announcement with four records");
        let message = Message::decode(&bytes).expect("a whole DNS message");
        assert!(message.response);
        assert_eq!(message.answers.len(), 4);
    }

    #[test]
    fn host_labels_stay_dns_clean() {
        assert_eq!(host_label("MacBook-de-Kenny"), "MacBook-de-Kenny");
        assert_eq!(host_label("Kenny's Mac Pro!"), "Kenny-s-Mac-Pro");
        assert_eq!(host_label("---"), "Mac");
        assert_eq!(host_label(""), "Mac");
        let long = "M".repeat(200);
        assert_eq!(host_label(&long).len(), 63);
    }
}
