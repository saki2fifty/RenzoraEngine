//! From-scratch UDP transport: a connection handshake plus a reliable,
//! de-duplicated message channel over `std::net::UdpSocket`.
//!
//! This is intentionally synchronous and Bevy-agnostic — the client/server
//! systems poll it once per frame. It replaces the previous lightyear stack:
//! the engine only needs reliable `GameEvent` (RPC) delivery + connection
//! lifecycle, so this does transport + lightweight netcode, **not** full state
//! replication (which was always TODO). Encryption is deliberately omitted for
//! now — add it before internet-facing production.

use std::collections::HashMap;
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
/// Maximum accepted encoded packet size. Receive buffers reserve one extra byte
/// so truncated oversized datagrams cannot masquerade as valid packets.
pub(crate) const MAX_DATAGRAM: usize = 4096;
const RELIABLE_WINDOW: usize = 1024;
pub(crate) const MAX_POLL_PACKETS: usize = 1024;

/// A message that was not accepted into the reliable send window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("reliable window is full; retry after acknowledgements arrive")]
    Backpressure,
    #[error("event exceeds the 4096-byte datagram limit")]
    TooLarge,
    #[error("sequence space exhausted; reconnect before sending more events")]
    SequenceExhausted,
    #[error("client is not connected")]
    NotConnected,
}

#[derive(Default)]
struct ReplayWindow {
    base: u64,
    bits: [u64; RELIABLE_WINDOW / 64],
}

impl ReplayWindow {
    // None means outside the admissible window: never acknowledge unseen data.
    fn receive(&mut self, seq: u32) -> Option<bool> {
        let seq = u64::from(seq);
        if seq < self.base {
            return Some(false);
        }
        if seq - self.base >= RELIABLE_WINDOW as u64 {
            return None;
        }
        let index = seq as usize % RELIABLE_WINDOW;
        let bit = 1u64 << (index % 64);
        if self.bits[index / 64] & bit != 0 {
            return Some(false);
        }
        self.bits[index / 64] |= bit;
        loop {
            let index = self.base as usize % RELIABLE_WINDOW;
            let bit = 1u64 << (index % 64);
            if self.bits[index / 64] & bit == 0 {
                break;
            }
            self.bits[index / 64] &= !bit;
            self.base += 1;
        }
        Some(true)
    }
}

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
    if bytes.len() > MAX_DATAGRAM {
        return None;
    }
    bincode::serde::decode_from_slice(
        bytes,
        bincode::config::standard().with_limit::<MAX_DATAGRAM>(),
    )
    .ok()
    .and_then(|(packet, used)| (used == bytes.len()).then_some(packet))
}

/// Per-peer reliability + liveness state. Used by both the client (one peer =
/// the server) and the server (one per connected client).
pub(crate) struct Peer {
    pub addr: SocketAddr,
    pub client_id: u64,
    next_seq: u64,
    send_base: u64,
    /// Encode once: every retry sends the same owned bytes until acknowledged.
    unacked: HashMap<u32, (Vec<u8>, Instant)>,
    seen: ReplayWindow,
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
            send_base: 0,
            unacked: HashMap::new(),
            seen: ReplayWindow::default(),
            last_recv: now,
            last_sent: now,
        }
    }

    fn raw_send(&mut self, socket: &UdpSocket, p: &Packet) {
        let _ = socket.send_to(&encode(p), self.addr);
        self.last_sent = Instant::now();
    }

    /// Queue + send a reliable event to this peer.
    pub fn send_reliable(&mut self, socket: &UdpSocket, event: GameEvent) -> Result<(), SendError> {
        if event.name.len().saturating_add(event.data.len()) > MAX_DATAGRAM {
            return Err(SendError::TooLarge);
        }
        let seq = u32::try_from(self.next_seq).map_err(|_| SendError::SequenceExhausted)?;
        if self.next_seq - self.send_base >= RELIABLE_WINDOW as u64 {
            return Err(SendError::Backpressure);
        }
        let bytes = encode(&Packet::Reliable { seq, event });
        if bytes.len() > MAX_DATAGRAM {
            return Err(SendError::TooLarge);
        }
        self.next_seq += 1;
        let _ = socket.send_to(&bytes, self.addr);
        let now = Instant::now();
        self.last_sent = now;
        self.unacked.insert(seq, (bytes, now));
        Ok(())
    }

    /// Acknowledge delivered/duplicate packets, never unseen out-of-window data.
    pub fn on_reliable(&mut self, socket: &UdpSocket, seq: u32) -> bool {
        let Some(deliver) = self.seen.receive(seq) else {
            return false;
        };
        self.raw_send(socket, &Packet::Ack { seq });
        deliver
    }

    /// Drop a reliable packet from the resend queue once acked.
    pub fn on_ack(&mut self, seq: u32) {
        self.unacked.remove(&seq);
        while self.send_base < self.next_seq && !self.unacked.contains_key(&(self.send_base as u32))
        {
            self.send_base += 1;
        }
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
    fn replay_window_bounds_history_and_preserves_late_duplicates() {
        let mut replay = ReplayWindow::default();
        assert_eq!(replay.receive(1024), None);
        for seq in (1..1024).rev() {
            assert_eq!(replay.receive(seq), Some(true));
        }
        assert_eq!(replay.base, 0);
        assert_eq!(replay.receive(0), Some(true));
        assert_eq!(replay.base, 1024);
        for seq in 1024..1_000_000 {
            assert_eq!(replay.receive(seq), Some(true));
        }
        assert_eq!(replay.receive(0), Some(false));
        assert_eq!(replay.receive(999_999), Some(false));
        assert_eq!(std::mem::size_of_val(&replay.bits), 128);
        assert!(replay.bits.iter().all(|word| *word == 0));
        replay.base = u64::from(u32::MAX);
        assert_eq!(replay.receive(u32::MAX), Some(true));
        assert_eq!(replay.base, u64::from(u32::MAX) + 1);
        assert_eq!(replay.receive(0), Some(false));
    }

    #[test]
    fn sender_window_does_not_advance_past_a_lost_oldest_packet() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut peer = Peer::new(socket.local_addr().unwrap(), 1);
        let event = || GameEvent {
            name: "test".into(),
            data: vec![1; 32],
        };
        for _ in 0..RELIABLE_WINDOW {
            peer.send_reliable(&socket, event()).unwrap();
        }
        let bytes: usize = peer.unacked.values().map(|(bytes, _)| bytes.len()).sum();
        assert!(bytes <= RELIABLE_WINDOW * MAX_DATAGRAM);
        for seq in 1..RELIABLE_WINDOW as u32 {
            peer.on_ack(seq);
        }
        assert_eq!(
            peer.send_reliable(&socket, event()),
            Err(SendError::Backpressure)
        );
        assert_eq!(peer.next_seq, RELIABLE_WINDOW as u64);
        peer.on_ack(0);
        peer.send_reliable(&socket, event()).unwrap();
        assert_eq!(peer.unacked.len(), 1);
        let next = peer.next_seq;
        assert_eq!(
            peer.send_reliable(
                &socket,
                GameEvent {
                    name: "large".into(),
                    data: vec![0; MAX_DATAGRAM]
                }
            ),
            Err(SendError::TooLarge)
        );
        assert_eq!(peer.next_seq, next);
        peer.unacked.clear();
        peer.next_seq = u64::from(u32::MAX);
        peer.send_base = peer.next_seq;
        peer.send_reliable(&socket, event()).unwrap();
        assert_eq!(
            peer.send_reliable(&socket, event()),
            Err(SendError::SequenceExhausted)
        );
        assert_eq!(peer.unacked.len(), 1);
    }

    #[test]
    fn decoder_rejects_trailing_and_oversized_data() {
        let mut packet = encode(&Packet::KeepAlive);
        assert!(decode(&packet).is_some());
        packet.push(0);
        assert!(decode(&packet).is_none());
        assert!(decode(&vec![0; MAX_DATAGRAM + 1]).is_none());
        let packet = encode(&Packet::Reliable {
            seq: 0,
            event: GameEvent {
                name: "large".into(),
                data: vec![1; 4000],
            },
        });
        assert!(decode(&packet).is_some());
    }

    #[test]
    fn reordering_and_lost_ack_do_not_deliver_a_retry_twice() {
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut sender = Peer::new(rx.local_addr().unwrap(), 1);
        let mut receiver = Peer::new(tx.local_addr().unwrap(), 2);
        for name in ["first", "second"] {
            sender
                .send_reliable(
                    &tx,
                    GameEvent {
                        name: name.into(),
                        data: Vec::new(),
                    },
                )
                .unwrap();
        }
        let mut buf = [0; MAX_DATAGRAM];
        let mut packets = Vec::new();
        for _ in 0..2 {
            let n = rx.recv(&mut buf).unwrap();
            packets.push(decode(&buf[..n]).unwrap());
        }
        let mut delivered = Vec::new();
        for packet in packets.into_iter().rev() {
            let Packet::Reliable { seq, event } = packet else {
                panic!("reliable expected")
            };
            if receiver.on_reliable(&rx, seq) {
                delivered.push(event.name);
            }
        }
        for _ in 0..2 {
            let n = tx.recv(&mut buf).unwrap();
            let Packet::Ack { seq } = decode(&buf[..n]).unwrap() else {
                panic!("ack expected")
            };
            if seq != 0 {
                sender.on_ack(seq);
            } // Simulate one lost acknowledgement.
        }
        assert_eq!(sender.unacked.len(), 1);
        let now = sender.unacked[&0].1 + RESEND_AFTER;
        sender.last_sent = now;
        sender.tick_at(&tx, now);
        let n = rx.recv(&mut buf).unwrap();
        let Packet::Reliable { seq, .. } = decode(&buf[..n]).unwrap() else {
            panic!("retry expected")
        };
        assert!(!receiver.on_reliable(&rx, seq));
        let n = tx.recv(&mut buf).unwrap();
        let Packet::Ack { seq } = decode(&buf[..n]).unwrap() else {
            panic!("ack expected")
        };
        sender.on_ack(seq);
        assert!(sender.unacked.is_empty());
        assert_eq!(delivered, ["second", "first"]);
    }

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
        )
        .unwrap();
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
