//! From-scratch UDP transport: a connection handshake plus a reliable,
//! de-duplicated message channel over `std::net::UdpSocket`.
//!
//! This is intentionally synchronous and Bevy-agnostic — the client/server
//! systems poll it once per frame. It replaces the previous lightyear stack:
//! the engine only needs reliable `GameEvent` (RPC) delivery + connection
//! lifecycle, so this does transport + lightweight netcode, **not** full state
//! replication (which was always TODO). Encryption is deliberately omitted for
//! now — add it before internet-facing production.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::messages::GameEvent;

/// Resend an unacked reliable packet after this long without an ack.
const RESEND_AFTER: Duration = Duration::from_millis(150);
/// Drop a peer we haven't heard from in this long.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(10);
/// Send a keep-alive if we haven't sent anything for this long.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(1);
/// Max UDP datagram we read into. RPC events are small; oversized payloads are
/// simply dropped by the socket (no fragmentation in v1).
pub(crate) const MAX_DATAGRAM: usize = 4096;

/// On-wire packet, bincode-encoded.
#[derive(Serialize, Deserialize)]
pub(crate) enum Packet {
    /// Client → server: request to join under a self-chosen id.
    ConnectRequest { client_id: u64 },
    /// Server → client: connection accepted.
    ConnectAccept { client_id: u64 },
    /// Either direction: graceful close.
    Disconnect,
    /// A reliable, de-duplicated application event.
    Reliable { seq: u32, event: GameEvent },
    /// Acknowledgement of a reliable packet.
    Ack { seq: u32 },
    /// Liveness probe (resets the peer timeout).
    KeepAlive,
}

pub(crate) fn encode(p: &Packet) -> Vec<u8> {
    bincode::serde::encode_to_vec(p, bincode::config::standard()).unwrap_or_default()
}

pub(crate) fn decode(bytes: &[u8]) -> Option<Packet> {
    bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .ok()
        .map(|(p, _)| p)
}

/// Per-peer reliability + liveness state. Used by both the client (one peer =
/// the server) and the server (one per connected client).
pub(crate) struct Peer {
    pub addr: SocketAddr,
    pub client_id: u64,
    next_seq: u32,
    /// Encode once: every retry sends the same owned bytes until acknowledged.
    unacked: HashMap<u32, (Vec<u8>, Instant)>,
    /// Session replay history. Bounding this requires sender flow control too:
    /// forgetting an old sequence could redeliver a late reliable retry.
    seen: HashSet<u32>,
    pub last_recv: Instant,
    last_sent: Instant,
}

impl Peer {
    pub fn new(addr: SocketAddr, client_id: u64) -> Self {
        let now = Instant::now();
        Self {
            addr,
            client_id,
            next_seq: 0,
            unacked: HashMap::new(),
            seen: HashSet::new(),
            last_recv: now,
            last_sent: now,
        }
    }

    fn raw_send(&mut self, socket: &UdpSocket, p: &Packet) {
        let _ = socket.send_to(&encode(p), self.addr);
        self.last_sent = Instant::now();
    }

    /// Queue + send a reliable event to this peer.
    pub fn send_reliable(&mut self, socket: &UdpSocket, event: GameEvent) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let bytes = encode(&Packet::Reliable { seq, event });
        let _ = socket.send_to(&bytes, self.addr);
        let now = Instant::now();
        self.last_sent = now;
        self.unacked.insert(seq, (bytes, now));
    }

    /// An incoming reliable packet arrived: always ack it, and return `true`
    /// the first time we see a given seq (i.e. deliver it once).
    pub fn on_reliable(&mut self, socket: &UdpSocket, seq: u32) -> bool {
        self.raw_send(socket, &Packet::Ack { seq });
        self.seen.insert(seq)
    }

    /// Drop a reliable packet from the resend queue once acked.
    pub fn on_ack(&mut self, seq: u32) {
        self.unacked.remove(&seq);
    }

    /// Per-frame upkeep: resend timed-out reliable packets + keep-alive.
    pub fn tick(&mut self, socket: &UdpSocket) {
        self.tick_at(socket, Instant::now());
    }

    fn tick_at(&mut self, socket: &UdpSocket, now: Instant) {
        let addr = self.addr;
        for (bytes, sent) in self.unacked.values_mut() {
            if now.duration_since(*sent) >= RESEND_AFTER {
                let _ = socket.send_to(bytes, addr);
                *sent = now;
            }
        }
        if now.duration_since(self.last_sent) >= KEEPALIVE_EVERY {
            self.raw_send(socket, &Packet::KeepAlive);
        }
    }

    pub fn timed_out(&self) -> bool {
        self.last_recv.elapsed() >= PEER_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_reuse_encoded_payload_and_ack_retires_it() {
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut peer = Peer::new(receiver.local_addr().unwrap(), 1);
        peer.send_reliable(
            &sender,
            GameEvent {
                name: "payload".into(),
                data: vec![42; 512],
            },
        );
        let mut buf = [0; MAX_DATAGRAM];
        let len = receiver.recv(&mut buf).unwrap();
        let original = buf[..len].to_vec();
        let allocation = peer.unacked[&0].0.as_ptr();
        let start = peer.unacked[&0].1;
        for retry in 1..=1_000 {
            let now = start + RESEND_AFTER * retry;
            // Isolate reliability from the independent keep-alive timer.
            peer.last_sent = now;
            peer.tick_at(&sender, now);
            let len = receiver.recv(&mut buf).unwrap();
            assert_eq!(&buf[..len], original.as_slice());
            assert_eq!(peer.unacked[&0].0.as_ptr(), allocation);
        }
        match decode(&original).unwrap() {
            Packet::Reliable { seq, event } => {
                assert_eq!(seq, 0);
                assert_eq!(event.name, "payload");
                assert_eq!(event.data, vec![42; 512]);
            }
            _ => panic!("expected reliable packet"),
        }
        peer.on_ack(0);
        assert!(peer.unacked.is_empty());
        assert!(peer.on_reliable(&sender, 42));
        assert!(!peer.on_reliable(&sender, 42));
    }
}
