// Copyright (c) 2019 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

pub mod errors;
pub mod handshake;
pub mod rate_limiter;

mod session;
mod timers;

use crate::noise::errors::WireGuardError;
use crate::noise::handshake::Handshake;
use crate::noise::rate_limiter::RateLimiter;
use crate::noise::timers::{TimerName, Timers};
use crate::x25519;

use std::collections::VecDeque;
use std::convert::{TryFrom, TryInto};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The default value to use for rate limiting, when no other rate limiter is defined
const PEER_HANDSHAKE_RATE_LIMIT: u64 = 10;

const IPV4_MIN_HEADER_SIZE: usize = 20;
const IPV4_LEN_OFF: usize = 2;
const IPV4_SRC_IP_OFF: usize = 12;
const IPV4_DST_IP_OFF: usize = 16;
const IPV4_IP_SZ: usize = 4;

const IPV6_MIN_HEADER_SIZE: usize = 40;
const IPV6_LEN_OFF: usize = 4;
const IPV6_SRC_IP_OFF: usize = 8;
const IPV6_DST_IP_OFF: usize = 24;
const IPV6_IP_SZ: usize = 16;

const IP_LEN_SZ: usize = 2;

const MAX_QUEUE_DEPTH: usize = 256;
/// number of sessions in the ring, better keep a PoT
const N_SESSIONS: usize = 8;

#[derive(Debug)]
pub enum TunnResult<'a> {
    Done,
    Err(WireGuardError),
    WriteToNetwork(&'a mut [u8]),
    WriteToTunnelV4(&'a mut [u8], Ipv4Addr),
    WriteToTunnelV6(&'a mut [u8], Ipv6Addr),
}

impl<'a> From<WireGuardError> for TunnResult<'a> {
    fn from(err: WireGuardError) -> TunnResult<'a> {
        TunnResult::Err(err)
    }
}

/// Tunnel represents a point-to-point WireGuard connection
pub struct Tunn {
    /// The handshake currently in progress
    handshake: handshake::Handshake,
    /// The N_SESSIONS most recent sessions, index is session id modulo N_SESSIONS
    sessions: [Option<session::Session>; N_SESSIONS],
    /// Index of most recently used session
    current: usize,
    /// Queue to store blocked packets
    packet_queue: VecDeque<Vec<u8>>,
    /// Keeps tabs on the expiring timers
    timers: timers::Timers,
    tx_bytes: usize,
    rx_bytes: usize,
    rate_limiter: Arc<RateLimiter>,
}

type MessageType = u32;
const HANDSHAKE_INIT: MessageType = 1;
const HANDSHAKE_RESP: MessageType = 2;
const COOKIE_REPLY: MessageType = 3;
const DATA: MessageType = 4;

const HANDSHAKE_INIT_SZ: usize = 148;
const HANDSHAKE_RESP_SZ: usize = 92;
const COOKIE_REPLY_SZ: usize = 64;
const DATA_OVERHEAD_SZ: usize = 32;

#[derive(Debug)]
pub struct HandshakeInit<'a> {
    sender_idx: u32,
    unencrypted_ephemeral: &'a [u8; 32],
    encrypted_static: &'a [u8],
    encrypted_timestamp: &'a [u8],
}

#[derive(Debug)]
pub struct HandshakeResponse<'a> {
    sender_idx: u32,
    pub receiver_idx: u32,
    unencrypted_ephemeral: &'a [u8; 32],
    encrypted_nothing: &'a [u8],
}

#[derive(Debug)]
pub struct PacketCookieReply<'a> {
    pub receiver_idx: u32,
    nonce: &'a [u8],
    encrypted_cookie: &'a [u8],
}

#[derive(Debug)]
pub struct PacketData<'a> {
    pub receiver_idx: u32,
    counter: u64,
    encrypted_encapsulated_packet: &'a [u8],
}

/// Describes a packet from network
#[derive(Debug)]
pub enum Packet<'a> {
    HandshakeInit(HandshakeInit<'a>),
    HandshakeResponse(HandshakeResponse<'a>),
    PacketCookieReply(PacketCookieReply<'a>),
    PacketData(PacketData<'a>),
}

impl Tunn {
    #[inline(always)]
    pub fn parse_incoming_packet(src: &[u8]) -> Result<Packet, WireGuardError> {
        if src.len() < 4 {
            return Err(WireGuardError::InvalidPacket);
        }

        // Checks the type, as well as the reserved zero fields
        let packet_type = u32::from_le_bytes(src[0..4].try_into().unwrap());

        Ok(match (packet_type, src.len()) {
            (HANDSHAKE_INIT, HANDSHAKE_INIT_SZ) => Packet::HandshakeInit(HandshakeInit {
                sender_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()),
                unencrypted_ephemeral: <&[u8; 32] as TryFrom<&[u8]>>::try_from(&src[8..40])
                    .expect("length already checked above"),
                encrypted_static: &src[40..88],
                encrypted_timestamp: &src[88..116],
            }),
            (HANDSHAKE_RESP, HANDSHAKE_RESP_SZ) => Packet::HandshakeResponse(HandshakeResponse {
                sender_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()),
                receiver_idx: u32::from_le_bytes(src[8..12].try_into().unwrap()),
                unencrypted_ephemeral: <&[u8; 32] as TryFrom<&[u8]>>::try_from(&src[12..44])
                    .expect("length already checked above"),
                encrypted_nothing: &src[44..60],
            }),
            (COOKIE_REPLY, COOKIE_REPLY_SZ) => Packet::PacketCookieReply(PacketCookieReply {
                receiver_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()),
                nonce: &src[8..32],
                encrypted_cookie: &src[32..64],
            }),
            (DATA, DATA_OVERHEAD_SZ..=usize::MAX) => Packet::PacketData(PacketData {
                receiver_idx: u32::from_le_bytes(src[4..8].try_into().unwrap()),
                counter: u64::from_le_bytes(src[8..16].try_into().unwrap()),
                encrypted_encapsulated_packet: &src[16..],
            }),
            _ => return Err(WireGuardError::InvalidPacket),
        })
    }

    pub fn is_expired(&self) -> bool {
        self.handshake.is_expired()
    }

    pub fn dst_address(packet: &[u8]) -> Option<IpAddr> {
        if packet.is_empty() {
            return None;
        }

        match packet[0] >> 4 {
            4 if packet.len() >= IPV4_MIN_HEADER_SIZE => {
                let addr_bytes: [u8; IPV4_IP_SZ] = packet
                    [IPV4_DST_IP_OFF..IPV4_DST_IP_OFF + IPV4_IP_SZ]
                    .try_into()
                    .unwrap();
                Some(IpAddr::from(addr_bytes))
            }
            6 if packet.len() >= IPV6_MIN_HEADER_SIZE => {
                let addr_bytes: [u8; IPV6_IP_SZ] = packet
                    [IPV6_DST_IP_OFF..IPV6_DST_IP_OFF + IPV6_IP_SZ]
                    .try_into()
                    .unwrap();
                Some(IpAddr::from(addr_bytes))
            }
            _ => None,
        }
    }

    /// Create a new tunnel using own private key and the peer public key
    #[deprecated(note = "Prefer `Tunn::new_at` to avoid time-impurity")]
    pub fn new(
        static_private: x25519::StaticSecret,
        peer_static_public: x25519::PublicKey,
        preshared_key: Option<[u8; 32]>,
        persistent_keepalive: Option<u16>,
        index: u32,
        rate_limiter: Option<Arc<RateLimiter>>,
    ) -> Self {
        Self::new_at(
            static_private,
            peer_static_public,
            preshared_key,
            persistent_keepalive,
            index,
            rate_limiter,
            rand::random(),
            Instant::now(),
        )
    }

    /// Create a new tunnel using own private key and the peer public key
    #[expect(clippy::too_many_arguments, reason = "We don't care that much.")]
    pub fn new_at(
        static_private: x25519::StaticSecret,
        peer_static_public: x25519::PublicKey,
        preshared_key: Option<[u8; 32]>,
        persistent_keepalive: Option<u16>,
        index: u32,
        rate_limiter: Option<Arc<RateLimiter>>,
        rng_seed: u64,
        now: Instant,
    ) -> Self {
        let static_public = x25519::PublicKey::from(&static_private);

        Tunn {
            handshake: Handshake::new(
                static_private,
                static_public,
                peer_static_public,
                index << 8,
                preshared_key,
                now,
            ),
            sessions: Default::default(),
            current: Default::default(),
            tx_bytes: Default::default(),
            rx_bytes: Default::default(),

            packet_queue: VecDeque::new(),
            timers: Timers::new(persistent_keepalive, rate_limiter.is_none(), rng_seed, now),

            rate_limiter: rate_limiter.unwrap_or_else(|| {
                Arc::new(RateLimiter::new_at(
                    &static_public,
                    PEER_HANDSHAKE_RATE_LIMIT,
                    now,
                ))
            }),
        }
    }

    /// Update the private key and clear existing sessions
    #[deprecated(note = "Prefer `Tunn::set_static_private_at` to avoid time-impurity")]
    pub fn set_static_private(
        &mut self,
        static_private: x25519::StaticSecret,
        static_public: x25519::PublicKey,
        rate_limiter: Option<Arc<RateLimiter>>,
    ) {
        self.set_static_private_at(static_private, static_public, rate_limiter, Instant::now())
    }

    /// Update the private key and clear existing sessions
    pub fn set_static_private_at(
        &mut self,
        static_private: x25519::StaticSecret,
        static_public: x25519::PublicKey,
        rate_limiter: Option<Arc<RateLimiter>>,
        now: Instant,
    ) {
        self.timers.should_reset_rr = rate_limiter.is_none();
        self.rate_limiter = rate_limiter.unwrap_or_else(|| {
            Arc::new(RateLimiter::new_at(
                &static_public,
                PEER_HANDSHAKE_RATE_LIMIT,
                now,
            ))
        });
        self.handshake
            .set_static_private(static_private, static_public);
        for s in &mut self.sessions {
            *s = None;
        }
    }

    /// Encapsulate a single packet from the tunnel interface.
    /// Returns TunnResult.
    ///
    /// # Panics
    /// Panics if dst buffer is too small.
    /// Size of dst should be at least src.len() + 32, and no less than 148 bytes.
    #[deprecated(note = "Prefer `Tunn::encapsulate_at` to avoid time-impurity")]
    pub fn encapsulate<'a>(&mut self, src: &[u8], dst: &'a mut [u8]) -> TunnResult<'a> {
        self.encapsulate_at(src, dst, Instant::now())
    }

    /// Encapsulate a single packet from the tunnel interface.
    /// Returns TunnResult.
    ///
    /// # Panics
    /// Panics if dst buffer is too small.
    /// Size of dst should be at least src.len() + 32, and no less than 148 bytes.
    pub fn encapsulate_at<'a>(
        &mut self,
        src: &[u8],
        dst: &'a mut [u8],
        now: Instant,
    ) -> TunnResult<'a> {
        let current = self.current;
        if let Some(session) = self.sessions[current % N_SESSIONS]
            .as_ref()
            .filter(|s| s.should_use_at(now) || self.timers.is_responder())
        {
            // Send the packet using an established session
            let packet = match session.format_packet_data(src, dst) {
                Ok(p) => p,
                Err(e) => return TunnResult::Err(e),
            };
            self.timer_tick(TimerName::TimeLastPacketSent, now);
            // Exclude Keepalive packets from timer update.
            if !src.is_empty() {
                self.timer_tick(TimerName::TimeLastDataPacketSent, now);
            }
            self.tx_bytes += src.len();
            return TunnResult::WriteToNetwork(packet);
        }

        // If there is no session, queue the packet for future retry
        self.queue_packet(src);
        // Initiate a new handshake if none is in progress
        self.format_handshake_initiation_at(dst, false, now)
    }

    /// Receives a UDP datagram from the network and parses it.
    /// Returns TunnResult.
    ///
    /// If the result is of type TunnResult::WriteToNetwork, should repeat the call with empty datagram,
    /// until TunnResult::Done is returned. If batch processing packets, it is OK to defer until last
    /// packet is processed.
    #[deprecated(note = "Prefer `Tunn::decapsulate_at` to avoid time-impurity")]
    pub fn decapsulate<'a>(
        &mut self,
        src_addr: Option<IpAddr>,
        datagram: &[u8],
        dst: &'a mut [u8],
    ) -> TunnResult<'a> {
        self.decapsulate_at(src_addr, datagram, dst, Instant::now())
    }

    /// Receives a UDP datagram from the network and parses it.
    /// Returns TunnResult.
    ///
    /// If the result is of type TunnResult::WriteToNetwork, should repeat the call with empty datagram,
    /// until TunnResult::Done is returned. If batch processing packets, it is OK to defer until last
    /// packet is processed.
    pub fn decapsulate_at<'a>(
        &mut self,
        src_addr: Option<IpAddr>,
        datagram: &[u8],
        dst: &'a mut [u8],
        now: Instant,
    ) -> TunnResult<'a> {
        if datagram.is_empty() {
            // Indicates a repeated call
            return self.send_queued_packet(dst, now);
        }

        let mut cookie = [0u8; COOKIE_REPLY_SZ];
        let packet = match self
            .rate_limiter
            .verify_packet_at(src_addr, datagram, &mut cookie, now)
        {
            Ok(packet) => packet,
            Err(TunnResult::WriteToNetwork(cookie)) => {
                dst[..cookie.len()].copy_from_slice(cookie);
                return TunnResult::WriteToNetwork(&mut dst[..cookie.len()]);
            }
            Err(TunnResult::Err(e)) => return TunnResult::Err(e),
            _ => unreachable!(),
        };

        self.handle_verified_packet(packet, dst, now)
    }

    pub(crate) fn handle_verified_packet<'a>(
        &mut self,
        packet: Packet,
        dst: &'a mut [u8],
        now: Instant,
    ) -> TunnResult<'a> {
        match packet {
            Packet::HandshakeInit(p) => self.handle_handshake_init(p, dst, now),
            Packet::HandshakeResponse(p) => self.handle_handshake_response(p, dst, now),
            Packet::PacketCookieReply(p) => self.handle_cookie_reply(p, now),
            Packet::PacketData(p) => self.handle_data(p, dst, now),
        }
        .unwrap_or_else(TunnResult::from)
    }

    fn handle_handshake_init<'a>(
        &mut self,
        p: HandshakeInit,
        dst: &'a mut [u8],
        now: Instant,
    ) -> Result<TunnResult<'a>, WireGuardError> {
        tracing::debug!(
            message = "Received handshake_initiation",
            remote_idx = p.sender_idx
        );

        let (packet, session) = self
            .handshake
            .receive_handshake_initialization(p, dst, now)?;

        // Store new session in ring buffer
        let index = session.local_index();
        self.sessions[index % N_SESSIONS] = Some(session);

        self.timer_tick(TimerName::TimeLastPacketReceived, now);
        self.timer_tick(TimerName::TimeLastPacketSent, now);
        self.timer_tick_session_established(false, now); // New session established, we are not the initiator

        tracing::debug!(message = "Sending handshake_response", local_idx = index);

        Ok(TunnResult::WriteToNetwork(packet))
    }

    fn handle_handshake_response<'a>(
        &mut self,
        p: HandshakeResponse,
        dst: &'a mut [u8],
        now: Instant,
    ) -> Result<TunnResult<'a>, WireGuardError> {
        tracing::debug!(
            message = "Received handshake_response",
            local_idx = p.receiver_idx,
            remote_idx = p.sender_idx
        );

        let session = self.handshake.receive_handshake_response(p, now)?;

        let keepalive_packet = session.format_packet_data(&[], dst)?;
        // Store new session in ring buffer
        let l_idx = session.local_index();
        let index = l_idx % N_SESSIONS;
        self.sessions[index] = Some(session);

        self.timer_tick(TimerName::TimeLastPacketReceived, now);
        self.timer_tick_session_established(true, now); // New session established, we are the initiator
        self.set_current_session(l_idx);

        tracing::debug!("Sending keepalive");

        Ok(TunnResult::WriteToNetwork(keepalive_packet)) // Send a keepalive as a response
    }

    fn handle_cookie_reply<'a>(
        &mut self,
        p: PacketCookieReply,
        now: Instant,
    ) -> Result<TunnResult<'a>, WireGuardError> {
        tracing::debug!(
            message = "Received cookie_reply",
            local_idx = p.receiver_idx
        );

        self.handshake.receive_cookie_reply(p, now)?;
        self.timer_tick(TimerName::TimeLastPacketReceived, now);

        tracing::debug!("Did set cookie");

        Ok(TunnResult::Done)
    }

    /// Update the index of the currently used session, if needed
    fn set_current_session(&mut self, new_idx: usize) {
        let cur_idx = self.current;
        if cur_idx == new_idx {
            // There is nothing to do, already using this session, this is the common case
            return;
        }

        let Some(new) = self.sessions[new_idx % N_SESSIONS].as_ref() else {
            debug_assert!(false, "new session should always exist");
            return;
        };
        if self.sessions[cur_idx % N_SESSIONS]
            .as_ref()
            .is_some_and(|current| current.established_at() > new.established_at())
        {
            // The current session is "newer" than the new one, don't update.
            return;
        }

        self.current = new_idx;
        tracing::debug!(message = "New session", session = new_idx);
    }

    /// Decrypts a data packet, and stores the decapsulated packet in dst.
    fn handle_data<'a>(
        &mut self,
        packet: PacketData,
        dst: &'a mut [u8],
        now: Instant,
    ) -> Result<TunnResult<'a>, WireGuardError> {
        let r_idx = packet.receiver_idx as usize;
        let idx = r_idx % N_SESSIONS;

        // Get the (probably) right session
        let decapsulated_packet = {
            let session = self.sessions[idx].as_ref();
            let session = session.ok_or_else(|| {
                tracing::trace!(message = "No current session available", remote_idx = r_idx);
                WireGuardError::NoCurrentSession
            })?;
            session.receive_packet_data(packet, dst)?
        };

        self.set_current_session(r_idx);

        self.timer_tick(TimerName::TimeLastPacketReceived, now);

        Ok(self.validate_decapsulated_packet(decapsulated_packet, now))
    }

    /// Formats a new handshake initiation message and store it in dst. If force_resend is true will send
    /// a new handshake, even if a handshake is already in progress (for example when a handshake times out)
    #[deprecated(note = "Prefer `Tunn::format_handshake_initiation_at` to avoid time-impurity")]
    pub fn format_handshake_initiation<'a>(
        &mut self,
        dst: &'a mut [u8],
        force_resend: bool,
    ) -> TunnResult<'a> {
        self.format_handshake_initiation_at(dst, force_resend, Instant::now())
    }

    /// Formats a new handshake initiation message and store it in dst. If force_resend is true will send
    /// a new handshake, even if a handshake is already in progress (for example when a handshake times out)
    pub fn format_handshake_initiation_at<'a>(
        &mut self,
        dst: &'a mut [u8],
        force_resend: bool,
        now: Instant,
    ) -> TunnResult<'a> {
        if self.handshake.is_in_progress() && !force_resend {
            return TunnResult::Done;
        }

        if self.handshake.is_expired() {
            self.timers.clear(now);
        }

        let starting_new_handshake = !self.handshake.is_in_progress();

        match self.handshake.format_handshake_initiation(dst, now) {
            Ok(packet) => {
                tracing::debug!("Sending handshake_initiation");

                if starting_new_handshake {
                    self.timer_tick(TimerName::TimeLastHandshakeStarted, now);
                }
                self.timer_tick(TimerName::TimeLastPacketSent, now);
                TunnResult::WriteToNetwork(packet)
            }
            Err(e) => TunnResult::Err(e),
        }
    }

    /// Check if an IP packet is v4 or v6, truncate to the length indicated by the length field
    /// Returns the truncated packet and the source IP as TunnResult
    fn validate_decapsulated_packet<'a>(
        &mut self,
        packet: &'a mut [u8],
        now: Instant,
    ) -> TunnResult<'a> {
        let (computed_len, src_ip_address) = match packet.len() {
            0 => return TunnResult::Done, // This is keepalive, and not an error
            _ if packet[0] >> 4 == 4 && packet.len() >= IPV4_MIN_HEADER_SIZE => {
                let len_bytes: [u8; IP_LEN_SZ] = packet[IPV4_LEN_OFF..IPV4_LEN_OFF + IP_LEN_SZ]
                    .try_into()
                    .unwrap();
                let addr_bytes: [u8; IPV4_IP_SZ] = packet
                    [IPV4_SRC_IP_OFF..IPV4_SRC_IP_OFF + IPV4_IP_SZ]
                    .try_into()
                    .unwrap();
                (
                    u16::from_be_bytes(len_bytes) as usize,
                    IpAddr::from(addr_bytes),
                )
            }
            _ if packet[0] >> 4 == 6 && packet.len() >= IPV6_MIN_HEADER_SIZE => {
                let len_bytes: [u8; IP_LEN_SZ] = packet[IPV6_LEN_OFF..IPV6_LEN_OFF + IP_LEN_SZ]
                    .try_into()
                    .unwrap();
                let addr_bytes: [u8; IPV6_IP_SZ] = packet
                    [IPV6_SRC_IP_OFF..IPV6_SRC_IP_OFF + IPV6_IP_SZ]
                    .try_into()
                    .unwrap();
                (
                    u16::from_be_bytes(len_bytes) as usize + IPV6_MIN_HEADER_SIZE,
                    IpAddr::from(addr_bytes),
                )
            }
            _ => return TunnResult::Err(WireGuardError::InvalidPacket),
        };

        if computed_len > packet.len() {
            return TunnResult::Err(WireGuardError::InvalidPacket);
        }

        self.timer_tick(TimerName::TimeLastDataPacketReceived, now);
        self.rx_bytes += computed_len;

        match src_ip_address {
            IpAddr::V4(addr) => TunnResult::WriteToTunnelV4(&mut packet[..computed_len], addr),
            IpAddr::V6(addr) => TunnResult::WriteToTunnelV6(&mut packet[..computed_len], addr),
        }
    }

    /// Get a packet from the queue, and try to encapsulate it
    fn send_queued_packet<'a>(&mut self, dst: &'a mut [u8], now: Instant) -> TunnResult<'a> {
        if let Some(packet) = self.dequeue_packet() {
            match self.encapsulate_at(&packet, dst, now) {
                TunnResult::Err(_) => {
                    // On error, return packet to the queue
                    self.requeue_packet(packet);
                }
                r => return r,
            }
        }
        TunnResult::Done
    }

    /// Push packet to the back of the queue
    fn queue_packet(&mut self, packet: &[u8]) {
        if self.packet_queue.len() < MAX_QUEUE_DEPTH {
            // Drop if too many are already in queue
            self.packet_queue.push_back(packet.to_vec());
        }
    }

    /// Push packet to the front of the queue
    fn requeue_packet(&mut self, packet: Vec<u8>) {
        if self.packet_queue.len() < MAX_QUEUE_DEPTH {
            // Drop if too many are already in queue
            self.packet_queue.push_front(packet);
        }
    }

    fn dequeue_packet(&mut self) -> Option<Vec<u8>> {
        self.packet_queue.pop_front()
    }

    fn estimate_loss(&self) -> f32 {
        let session_idx = self.current;

        let mut weight = 9.0;
        let mut cur_avg = 0.0;
        let mut total_weight = 0.0;

        for i in 0..N_SESSIONS {
            if let Some(ref session) = self.sessions[(session_idx.wrapping_sub(i)) % N_SESSIONS] {
                let (expected, received) = session.current_packet_cnt();

                let loss = if expected == 0 {
                    0.0
                } else {
                    1.0 - received as f32 / expected as f32
                };

                cur_avg += loss * weight;
                total_weight += weight;
                weight /= 3.0;
            }
        }

        if total_weight == 0.0 {
            0.0
        } else {
            cur_avg / total_weight
        }
    }

    /// Return stats from the tunnel:
    /// * Time since last handshake in seconds
    /// * Data bytes sent
    /// * Data bytes received
    #[deprecated(note = "Prefer `Tunn::stats_at` to avoid time-impurity")]
    pub fn stats(&self) -> (Option<Duration>, usize, usize, f32, Option<u32>) {
        self.stats_at(Instant::now())
    }

    /// Return stats from the tunnel:
    /// * Time since last handshake in seconds
    /// * Data bytes sent
    /// * Data bytes received
    pub fn stats_at(&self, now: Instant) -> (Option<Duration>, usize, usize, f32, Option<u32>) {
        let time = self.time_since_last_handshake_at(now);
        let tx_bytes = self.tx_bytes;
        let rx_bytes = self.rx_bytes;
        let loss = self.estimate_loss();
        let rtt = self.handshake.last_rtt;

        (time, tx_bytes, rx_bytes, loss, rtt)
    }
}

#[cfg(test)]
mod tests {
    use crate::noise::timers::{REKEY_AFTER_TIME, REKEY_TIMEOUT};
    use std::time::Instant;

    use super::*;
    use rand::{rngs::OsRng, RngCore};
    use timers::{KEEPALIVE_TIMEOUT, MAX_JITTER, REJECT_AFTER_TIME, SHOULD_NOT_USE_AFTER_TIME};
    use tracing::{level_filters::LevelFilter, Level};
    use tracing_subscriber::util::SubscriberInitExt;

    fn create_two_tuns(now: Instant) -> (Tunn, Tunn) {
        let my_secret_key = x25519_dalek::StaticSecret::random_from_rng(OsRng);
        let my_public_key = x25519_dalek::PublicKey::from(&my_secret_key);
        let my_idx = OsRng.next_u32();

        let their_secret_key = x25519_dalek::StaticSecret::random_from_rng(OsRng);
        let their_public_key = x25519_dalek::PublicKey::from(&their_secret_key);
        let their_idx = OsRng.next_u32();

        let my_tun = Tunn::new_at(
            my_secret_key,
            their_public_key,
            None,
            None,
            my_idx,
            None,
            rand::random(),
            now,
        );

        let their_tun = Tunn::new_at(
            their_secret_key,
            my_public_key,
            None,
            None,
            their_idx,
            None,
            rand::random(),
            now,
        );

        (my_tun, their_tun)
    }

    fn create_handshake_init(tun: &mut Tunn, now: Instant) -> Vec<u8> {
        let mut dst = vec![0u8; 2048];
        let handshake_init = tun.format_handshake_initiation_at(&mut dst, false, now);
        assert!(matches!(handshake_init, TunnResult::WriteToNetwork(_)));
        let handshake_init = if let TunnResult::WriteToNetwork(sent) = handshake_init {
            sent
        } else {
            unreachable!();
        };

        handshake_init.into()
    }

    fn create_handshake_response(tun: &mut Tunn, handshake_init: &[u8], now: Instant) -> Vec<u8> {
        let mut dst = vec![0u8; 2048];
        let handshake_resp = tun.decapsulate_at(None, handshake_init, &mut dst, now);
        assert!(matches!(handshake_resp, TunnResult::WriteToNetwork(_)));

        let handshake_resp = if let TunnResult::WriteToNetwork(sent) = handshake_resp {
            sent
        } else {
            unreachable!();
        };

        handshake_resp.into()
    }

    fn parse_handshake_resp(tun: &mut Tunn, handshake_resp: &[u8], now: Instant) -> Vec<u8> {
        let mut dst = vec![0u8; 2048];
        let keepalive = tun.decapsulate_at(None, handshake_resp, &mut dst, now);
        assert!(matches!(keepalive, TunnResult::WriteToNetwork(_)));

        let keepalive = if let TunnResult::WriteToNetwork(sent) = keepalive {
            sent
        } else {
            unreachable!();
        };

        keepalive.into()
    }

    fn parse_keepalive(tun: &mut Tunn, keepalive: &[u8], now: Instant) {
        let mut dst = vec![0u8; 2048];
        let keepalive = tun.decapsulate_at(None, keepalive, &mut dst, now);
        assert!(matches!(keepalive, TunnResult::Done));
    }

    fn create_two_tuns_and_handshake(now: Instant) -> (Tunn, Tunn) {
        let (mut my_tun, mut their_tun) = create_two_tuns(now);
        let init = create_handshake_init(&mut my_tun, now);
        let resp = create_handshake_response(&mut their_tun, &init, now);
        let keepalive = parse_handshake_resp(&mut my_tun, &resp, now);
        parse_keepalive(&mut their_tun, &keepalive, now);

        (my_tun, their_tun)
    }

    fn create_ipv4_udp_packet() -> Vec<u8> {
        let header =
            etherparse::PacketBuilder::ipv4([192, 168, 1, 2], [192, 168, 1, 3], 5).udp(5678, 23);
        let payload = [0, 1, 2, 3];
        let mut packet = Vec::<u8>::with_capacity(header.size(payload.len()));
        header.write(&mut packet, &payload).unwrap();
        packet
    }

    fn update_timer_results_in_handshake(tun: &mut Tunn, now: &mut Instant) {
        for _ in 0..=MAX_JITTER.as_millis() {
            *now += Duration::from_millis(1);

            let mut dst = vec![0u8; 2048];
            let TunnResult::WriteToNetwork(packet_data) = tun.update_timers_at(&mut dst, *now)
            else {
                continue;
            };

            let packet = Tunn::parse_incoming_packet(packet_data).unwrap();
            assert!(matches!(packet, Packet::HandshakeInit(_)));

            return;
        }

        panic!("Handshake was not sent within jitter duration")
    }

    #[test]
    fn create_two_tunnels_linked_to_eachother() {
        let now = Instant::now();

        let (_my_tun, _their_tun) = create_two_tuns(now);
    }

    #[test]
    fn handshake_init() {
        let now = Instant::now();

        let (mut my_tun, _their_tun) = create_two_tuns(now);
        let init = create_handshake_init(&mut my_tun, now);
        let packet = Tunn::parse_incoming_packet(&init).unwrap();
        assert!(matches!(packet, Packet::HandshakeInit(_)));
    }

    #[test]
    fn handshake_init_and_response() {
        let now = Instant::now();

        let (mut my_tun, mut their_tun) = create_two_tuns(now);
        let init = create_handshake_init(&mut my_tun, now);
        let resp = create_handshake_response(&mut their_tun, &init, now);
        let packet = Tunn::parse_incoming_packet(&resp).unwrap();
        assert!(matches!(packet, Packet::HandshakeResponse(_)));
    }

    #[test]
    fn full_handshake() {
        let now = Instant::now();

        let (mut my_tun, mut their_tun) = create_two_tuns(now);
        let init = create_handshake_init(&mut my_tun, now);
        let resp = create_handshake_response(&mut their_tun, &init, now);
        let keepalive = parse_handshake_resp(&mut my_tun, &resp, now);
        let packet = Tunn::parse_incoming_packet(&keepalive).unwrap();
        assert!(matches!(packet, Packet::PacketData(_)));
    }

    #[test]
    fn full_handshake_plus_timers() {
        let now = Instant::now();

        let (mut my_tun, mut their_tun) = create_two_tuns_and_handshake(now);
        // Time has not yet advanced so their is nothing to do
        assert!(matches!(
            my_tun.update_timers_at(&mut [], now),
            TunnResult::Done
        ));
        assert!(matches!(
            their_tun.update_timers_at(&mut [], now),
            TunnResult::Done
        ));
    }

    #[test]
    fn multiple_update_calls_no_duplicate_handshakes() {
        let mut now = Instant::now();
        let mut num_handshakes_initiator = 0;
        let mut num_handshakes_responder = 0;
        let mut buf = [0u8; 200];

        let (mut my_tun, mut their_tun) = create_two_tuns_and_handshake(now);

        now += REJECT_AFTER_TIME;
        my_tun.update_timers_at(&mut buf, now);
        their_tun.update_timers_at(&mut buf, now);

        for _ in 0..200 {
            now += Duration::from_millis(10);

            if matches!(
                my_tun.update_timers_at(&mut buf, now),
                TunnResult::WriteToNetwork(_)
            ) {
                num_handshakes_initiator += 1;
            }
            if matches!(
                their_tun.update_timers_at(&mut buf, now),
                TunnResult::WriteToNetwork(_)
            ) {
                num_handshakes_responder += 1;
            }
        }

        assert_eq!(num_handshakes_initiator, 1);
        assert_eq!(num_handshakes_responder, 0);
    }

    #[test]
    fn new_handshake_after_two_mins() {
        let mut now = Instant::now();

        let (mut my_tun, mut their_tun) = create_two_tuns_and_handshake(now);
        let mut my_dst = [0u8; 1024];

        // Advance time 1 second and "send" 1 packet so that we send a handshake
        // after the timeout

        now += Duration::from_secs(1);

        assert!(matches!(
            their_tun.update_timers_at(&mut [], now),
            TunnResult::Done
        ));
        assert!(matches!(
            my_tun.update_timers_at(&mut my_dst, now),
            TunnResult::Done
        ));
        let sent_packet_buf = create_ipv4_udp_packet();
        let data = my_tun.encapsulate_at(&sent_packet_buf, &mut my_dst, now);
        assert!(matches!(data, TunnResult::WriteToNetwork(_)));

        //Advance to timeout
        now += REKEY_AFTER_TIME;

        assert!(matches!(
            their_tun.update_timers_at(&mut [], now),
            TunnResult::Done
        ));
        update_timer_results_in_handshake(&mut my_tun, &mut now);
    }

    /// If a tunnel is idle for close to 120s without sending a packet,
    /// no new handshake is performed by the initiator.
    /// This can lead to a race-condition where the sender sends a packet on an almost expired session
    /// and by the time it is received, the session is expired.
    #[test]
    fn new_handshake_on_packet_for_session_that_is_about_to_expire() {
        let mut now = Instant::now();

        let (mut my_tun, _their_tun) = create_two_tuns_and_handshake(now);
        let mut my_dst = [0u8; 1024];

        now += SHOULD_NOT_USE_AFTER_TIME + Duration::from_secs(1);

        let sent_packet_buf = create_ipv4_udp_packet();
        let data = my_tun.encapsulate_at(&sent_packet_buf, &mut my_dst, now);

        let TunnResult::WriteToNetwork(data) = data else {
            panic!("Expected `WriteToNetwork`")
        };

        assert!(matches!(
            Tunn::parse_incoming_packet(data).unwrap(),
            Packet::HandshakeInit(_)
        ));
    }

    #[test]
    fn responder_can_still_use_almost_expired_session() {
        let mut now = Instant::now();

        let (_initiator_tun, mut resonder_tun) = create_two_tuns_and_handshake(now);
        let mut responder_tun = [0u8; 1024];

        now += SHOULD_NOT_USE_AFTER_TIME + Duration::from_secs(1);

        let sent_packet_buf = create_ipv4_udp_packet();
        let data = resonder_tun.encapsulate_at(&sent_packet_buf, &mut responder_tun, now);

        let TunnResult::WriteToNetwork(data) = data else {
            panic!("Expected `WriteToNetwork`")
        };

        assert!(matches!(
            Tunn::parse_incoming_packet(data).unwrap(),
            Packet::PacketData(_)
        ));
    }

    #[test]
    fn handshake_no_resp_rekey_timeout() {
        let mut now = Instant::now();

        let (mut my_tun, _their_tun) = create_two_tuns(now);

        let init = create_handshake_init(&mut my_tun, now);
        let packet = Tunn::parse_incoming_packet(&init).unwrap();
        assert!(matches!(packet, Packet::HandshakeInit(_)));

        now += REKEY_TIMEOUT;
        update_timer_results_in_handshake(&mut my_tun, &mut now)
    }

    #[test]
    fn one_ip_packet() {
        let (mut my_tun, mut their_tun) = create_two_tuns_and_handshake(Instant::now());
        let mut my_dst = [0u8; 1024];
        let mut their_dst = [0u8; 1024];

        let sent_packet_buf = create_ipv4_udp_packet();

        let data = my_tun.encapsulate_at(&sent_packet_buf, &mut my_dst, Instant::now());
        assert!(matches!(data, TunnResult::WriteToNetwork(_)));
        let data = if let TunnResult::WriteToNetwork(sent) = data {
            sent
        } else {
            unreachable!();
        };

        let data = their_tun.decapsulate_at(None, data, &mut their_dst, Instant::now());
        assert!(matches!(data, TunnResult::WriteToTunnelV4(..)));
        let recv_packet_buf = if let TunnResult::WriteToTunnelV4(recv, _addr) = data {
            recv
        } else {
            unreachable!();
        };
        assert_eq!(sent_packet_buf, recv_packet_buf);
    }

    #[test]
    fn silent_without_application_traffic_and_persistent_keepalive() {
        let _guard = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(LevelFilter::DEBUG)
            .set_default();

        let mut now = Instant::now();

        let (mut my_tun, mut their_tun) = create_two_tuns_and_handshake(now);
        assert_eq!(my_tun.persistent_keepalive(), None);
        their_tun.set_persistent_keepalive(10);

        let mut my_dst = [0u8; 1024];
        let mut their_dst = [0u8; 1024];

        now += Duration::from_secs(1);

        let sent_packet_buf = create_ipv4_udp_packet();

        // First, perform an application-level handshake.

        {
            // Send the request.

            let data = my_tun
                .encapsulate_at(&sent_packet_buf, &mut my_dst, now)
                .unwrap_network();

            now += Duration::from_secs(1);

            let data = their_tun.decapsulate_at(None, data, &mut their_dst, now);
            assert!(matches!(data, TunnResult::WriteToTunnelV4(..)));
        }

        now += Duration::from_secs(1);

        {
            // Send the response.

            let data = their_tun
                .encapsulate_at(&sent_packet_buf, &mut their_dst, now)
                .unwrap_network();

            now += Duration::from_secs(1);

            let data = my_tun.decapsulate_at(None, data, &mut my_dst, now);
            assert!(matches!(data, TunnResult::WriteToTunnelV4(..)));
        }

        // Wait for `KEEPALIVE_TIMEOUT`.

        now += KEEPALIVE_TIMEOUT;

        let keepalive = my_tun.update_timers_at(&mut my_dst, now).unwrap_network();
        parse_keepalive(&mut their_tun, keepalive, now);

        // Idle for 60 seconds.

        for _ in 0..60 {
            now += Duration::from_secs(1);

            // `my_tun` stays silent (i.e. does not respond to keepalives with keepalives).
            assert!(matches!(
                my_tun.update_timers_at(&mut my_dst, now),
                TunnResult::Done
            ));

            // `their_tun` will emit persistent keep-alives as we idle.
            match their_tun.update_timers_at(&mut their_dst, now) {
                TunnResult::Done => {}
                TunnResult::Err(wire_guard_error) => panic!("{wire_guard_error}"),
                TunnResult::WriteToNetwork(keepalive) => {
                    parse_keepalive(&mut my_tun, keepalive, now)
                }
                TunnResult::WriteToTunnelV4(_, _) | TunnResult::WriteToTunnelV6(_, _) => {
                    unreachable!()
                }
            }
        }
    }

    #[test]
    fn rekey_without_response() {
        let _guard = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(Level::DEBUG)
            .try_init();

        let mut now = Instant::now();
        let sent_packet_buf = create_ipv4_udp_packet();

        let (mut my_tun, mut their_tun) = create_two_tuns_and_handshake(now);
        let mut my_dst = [0u8; 1024];
        let mut their_dst = [0u8; 1024];

        now += Duration::from_secs(1);

        // Simulate an application-level handshake.
        let req = my_tun
            .encapsulate_at(&sent_packet_buf, &mut my_dst, now)
            .unwrap_network();
        their_tun.decapsulate_at(None, req, &mut their_dst, now);
        let res = their_tun
            .encapsulate_at(&sent_packet_buf, &mut their_dst, now)
            .unwrap_network();
        my_tun.decapsulate_at(None, res, &mut my_dst, now);

        // Idle the connection for 10s.
        now += Duration::from_secs(10);

        let first_unreplied_packet_sent = now;

        // Start sending more traffic each second, this time without a reply.
        for _ in 0..10 {
            my_tun.encapsulate_at(&sent_packet_buf, &mut my_dst, now);
            now += Duration::from_secs(1);

            assert!(
                matches!(my_tun.update_timers_at(&mut [], now), TunnResult::Done),
                "No time based action should be necessary yet"
            )
        }

        // Timeout should be from the first unreplied packet.
        let rekey_at = first_unreplied_packet_sent + KEEPALIVE_TIMEOUT + REKEY_TIMEOUT;

        // Trigger the creation of a handshake.
        // Will be scheduled with 0..333ms
        assert!(matches!(
            my_tun.update_timers_at(&mut [], rekey_at),
            TunnResult::Done
        ));

        let TunnResult::WriteToNetwork(handshake) =
            my_tun.update_timers_at(&mut my_dst, rekey_at + MAX_JITTER)
        else {
            panic!("Expected handshake")
        };

        assert!(matches!(
            Tunn::parse_incoming_packet(handshake).unwrap(),
            Packet::HandshakeInit(_)
        ));
    }

    impl<'a> TunnResult<'a> {
        fn unwrap_network(self) -> &'a [u8] {
            match self {
                TunnResult::Done => panic!("Expected `WriteToNetwork` but was `Done`"),
                TunnResult::Err(e) => panic!("Expected `WriteToNetwork` but was `Err({e:?})`"),
                TunnResult::WriteToNetwork(d) => d,
                TunnResult::WriteToTunnelV4(_, _) => {
                    panic!("Expected `WriteToNetwork` but was `WriteToTunnelV4`")
                }
                TunnResult::WriteToTunnelV6(_, _) => {
                    panic!("Expected `WriteToNetwork` but was `WriteToTunnelV6`")
                }
            }
        }
    }
}
