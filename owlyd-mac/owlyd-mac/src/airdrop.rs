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
//!
//! v0.1.6, one lane more: the v06 field run proved macOS never routed a
//! single multicast packet into the utun ("0 paquets lus au total" while
//! ~1700 announcements left the socket, every send's error thrown away).
//! The socket stays — queries arrive on it and answers leave on it, and
//! its send errors are said out loud now — but the announcements also
//! ride the loop-in lane: the same bursts, pushed straight into the
//! engine's netif channel, as if the kernel had routed them. A phone that
//! receives both copies has merely one announcement more of the 64 each
//! burst sends anyway.

use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::time::Instant;

use owly_airdrop::driver::mdns::{Mdns, Registration};
use owly_airdrop::driver::receiver::{Options, Receiver, ReceiverEvent};
use owly_airdrop::driver::tls::{rustls, Identity};
use owly_airdrop::proto::mdns::{service_id, Announcer, Burst, MDNS_GROUP, MDNS_PORT, Service, RECEIVER_FLAGS};
use owly_airdrop::proto::receiver::Config;
use owly_awdl::Ipv6Packet;

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
pub async fn start(
    scope: u32,
    addr: Ipv6Addr,
    name: &str,
    host: mpsc::Sender<Ipv6Packet>,
) -> io::Result<AirDrop> {
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
        .register(service.clone())
        .await
        .map_err(|e| io::Error::other(format!("le répondeur mDNS sur l'interface {scope} : {e}")))?;

    // The loop-in lane: the same bursts, straight into the engine — the
    // ride the kernel refuses to give the socket on this utun (v06: zero
    // packets ever read off it). The socket keeps its own burst for the
    // day the kernel carries it; a phone that sees both copies of an
    // announcement has one more of the 64 a burst sends anyway.
    let packet = Ipv6Packet::from_ip(ipv6_udp(
        addr,
        MDNS_GROUP,
        MDNS_PORT,
        MDNS_PORT,
        &service.announcement(),
    ))
    .ok_or_else(|| io::Error::other("l'annonce ne se construit pas (adresse ?)"))?;
    tokio::spawn(async move {
        let mut announcer = Announcer::new(Some(ANNOUNCE), Instant::now());
        while let Some(at) = announcer.next_timer() {
            tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
            if announcer.on_timer(Instant::now())
                && host.send(packet.clone()).await.is_err()
            {
                return; // the engine is gone
            }
        }
    });
    println!(
        "== annonces _airdrop._tcp : chemin direct vers le moteur ouvert (64 copies par salve, \
         tous les 3 s)"
    );

    println!(
        "== AirDrop : « {name} » (id {id}) annoncé sur [{addr}%{scope}]:{AIRDROP_PORT}, \
         drapeaux {RECEIVER_FLAGS:#05x} — il apparaîtra dans les feuilles de partage"
    );
    Ok(AirDrop {
        events,
        _announce: announce,
    })
}

/// One UDP datagram inside IPv6, as the kernel would have built it for a
/// link-local multicast: hop limit 255 (mDNS's link-local rule), and the
/// checksum IPv6 refuses to travel without (RFC 8200 §8.1 — UDP over
/// IPv6 has no checksum-less option, so the loop-in lane carries its
/// own).
fn ipv6_udp(src: Ipv6Addr, dst: Ipv6Addr, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    const UDP: u8 = 17;
    let ulen = (8 + payload.len()) as u16;
    let mut udp = Vec::with_capacity(8 + payload.len());
    udp.extend_from_slice(&sport.to_be_bytes());
    udp.extend_from_slice(&dport.to_be_bytes());
    udp.extend_from_slice(&ulen.to_be_bytes());
    udp.extend_from_slice(&[0, 0]); // checksum, filled below
    udp.extend_from_slice(payload);
    let sum = internet_checksum(src, dst, UDP, &udp);
    let mut pkt = Vec::with_capacity(40 + udp.len());
    pkt.extend_from_slice(&[0x60, 0, 0, 0]);
    pkt.extend_from_slice(&ulen.to_be_bytes());
    pkt.push(UDP);
    pkt.push(255); // hop limit: link-local multicast wants 255
    pkt.extend_from_slice(&src.octets());
    pkt.extend_from_slice(&dst.octets());
    pkt.extend_from_slice(&udp);
    let at = pkt.len() - udp.len() + 6;
    pkt[at..at + 2].copy_from_slice(&sum.to_be_bytes());
    pkt
}

/// The Internet checksum over the IPv6 pseudo-header and `message` —
/// owly-awdl carries the same one for its ICMPv6; this is the daemon's
/// copy for the lane that builds its own packets.
fn internet_checksum(src: Ipv6Addr, dst: Ipv6Addr, next: u8, message: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut add = |b: &[u8]| {
        for pair in b.chunks(2) {
            let hi = pair[0] as u32;
            let lo = pair.get(1).copied().unwrap_or(0) as u32;
            sum += (hi << 8) | lo;
        }
    };
    add(&src.octets());
    add(&dst.octets());
    add(&(message.len() as u32).to_be_bytes());
    add(&[0, 0, 0, next]);
    add(message);
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
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

    /// The loop-in lane's packet, whole: the engine's own parser takes it
    /// (link-local source, `33:33:00:00:00:fb` from the group), the hop
    /// limit is mDNS's 255, and a correct checksum sums, with itself, to
    /// zero — the property RFC 1071 hangs on it.
    #[test]
    fn the_loop_in_packet_is_a_packet() {
        let src: Ipv6Addr = "fe80::b0d9:efff:fe64:a87c".parse().unwrap();
        let pkt = ipv6_udp(src, MDNS_GROUP, MDNS_PORT, MDNS_PORT, b"an mDNS announcement");
        let parsed = Ipv6Packet::from_ip(pkt.clone()).expect("the engine's parser takes it");
        // `33:33:00:00:00:fb` for the group, and the link-local folds back
        // to the AWDL address it was made from (the 0x02 bit restored).
        assert_eq!(parsed.src.0, [0xb2, 0xd9, 0xef, 0x64, 0xa8, 0x7c]);
        assert_eq!(parsed.dst.0, [0x33, 0x33, 0, 0, 0, 0xfb]);
        assert_eq!(pkt[7], 255, "hop limit");
        assert_eq!(u16::from_be_bytes([pkt[4], pkt[5]]), 28, "payload length");
        // A correct UDP checksum over the pseudo-header sums to zero with
        // itself (the checksum field included, as it travels).
        let udp = &pkt[40..];
        assert_eq!(internet_checksum(src, MDNS_GROUP, 17, udp), 0);
    }
}
