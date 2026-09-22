use crate::{
    build_encrypted_realtime_packet, build_ntp_sync_packet, build_ptp_sync_packet,
    encode_alac_16_stereo_352, AlacEncodeError, DatagramSendOutcome, MediaTransport,
    MediaTransportError, NtpSyncPacketArgs, PtpClock, PtpExchange, PtpSyncPacketArgs,
    RetransmitRing, RtpState,
    ALAC_PCM_PACKET_BYTES, FRAMES_PER_PACKET_44100,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum MediaSendError {
    Transport(MediaTransportError),
    Packet(crate::AudioPacketError),
    Alac(AlacEncodeError),
}

impl From<MediaTransportError> for MediaSendError {
    fn from(value: MediaTransportError) -> Self { Self::Transport(value) }
}
impl From<crate::AudioPacketError> for MediaSendError {
    fn from(value: crate::AudioPacketError) -> Self { Self::Packet(value) }
}
impl From<AlacEncodeError> for MediaSendError {
    fn from(value: AlacEncodeError) -> Self { Self::Alac(value) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaSendResult {
    pub sequence_sent: u16,
    pub timestamp_sent: u32,
    pub sync_sent: bool,
    pub audio_delivered: bool,
    pub first_marker: bool,
}

#[derive(Debug, Clone)]
enum RealtimeTiming {
    Ntp,
    Ptp { clock: PtpClock },
}

pub struct RealtimeMediaSender {
    transport: MediaTransport,
    state: RtpState,
    audio_key: [u8; 32],
    timing: RealtimeTiming,
    ptp_anchor_wall0: Option<u64>,
    ptp_anchor_pos0: u32,
    head_ts: u64,
    pacing_window_frames: u64,
    pace_last_release: Option<Instant>,
    pacing_enabled: bool,
    retransmit: Option<RetransmitRing>,
    splice_pad_frames: u32,
    timeline_reanchors: u64,
    reanchor_shifted_frames: u64,
}

impl RealtimeMediaSender {
    pub fn new(transport: MediaTransport, state: RtpState, audio_key: [u8; 32]) -> Self {
        Self {
            transport,
            state,
            audio_key,
            timing: RealtimeTiming::Ntp,
            ptp_anchor_wall0: None,
            ptp_anchor_pos0: state.timestamp,
            head_ts: state.timestamp as u64,
            pacing_window_frames: 0,
            pace_last_release: None,
            pacing_enabled: false,
            retransmit: None,
            splice_pad_frames: 0,
            timeline_reanchors: 0,
            reanchor_shifted_frames: 0,
        }
    }

    pub fn new_ptp(
        transport: MediaTransport,
        state: RtpState,
        audio_key: [u8; 32],
        clock_id: u64,
    ) -> Self {
        Self {
            transport,
            state,
            audio_key,
            timing: RealtimeTiming::Ptp { clock: PtpClock::fixed(clock_id) },
            ptp_anchor_wall0: None,
            ptp_anchor_pos0: state.timestamp,
            head_ts: state.timestamp as u64,
            pacing_window_frames: 0,
            pace_last_release: None,
            pacing_enabled: false,
            retransmit: None,
            splice_pad_frames: 0,
            timeline_reanchors: 0,
            reanchor_shifted_frames: 0,
        }
    }

    pub fn new_ptp_clock(
        transport: MediaTransport,
        state: RtpState,
        audio_key: [u8; 32],
        clock: PtpClock,
    ) -> Self {
        Self {
            transport,
            state,
            audio_key,
            timing: RealtimeTiming::Ptp { clock },
            ptp_anchor_wall0: None,
            ptp_anchor_pos0: state.timestamp,
            head_ts: state.timestamp as u64,
            pacing_window_frames: 0,
            pace_last_release: None,
            pacing_enabled: false,
            retransmit: None,
            splice_pad_frames: 0,
            timeline_reanchors: 0,
            reanchor_shifted_frames: 0,
        }
    }

    pub fn set_retransmit_ring(&mut self, ring: RetransmitRing) {
        self.retransmit = Some(ring);
    }

    pub fn configure_source_timeline(
        &mut self,
        start_ntp: u64,
        head_ts: u64,
        latency_max: Option<u32>,
        lead_frames: u32,
    ) {
        const PACING_MARGIN_FRAMES: u64 = 11_025; // 250 ms @ 44.1 kHz
        const DEFAULT_BUFFER_WINDOW: u64 = 77_175; // (2000 - 250) ms
        const SPLICE_DEPTH_FRAMES: u64 = 26_460; // 600 ms

        let reported = latency_max
            .map(|v| v as u64)
            .filter(|v| *v > PACING_MARGIN_FRAMES);
        let receiver_window = reported
            .map(|v| v - PACING_MARGIN_FRAMES)
            .unwrap_or(DEFAULT_BUFFER_WINDOW);

        self.head_ts = head_ts;
        self.pacing_window_frames = receiver_window.min(SPLICE_DEPTH_FRAMES);
        self.pace_last_release = None;
        self.pacing_enabled = true;

        if let RealtimeTiming::Ptp { clock } = &self.timing {
            let lead_ns = frames_to_ns(lead_frames);
            let local_now = system_unix_ns();
            let master_now = clock.master_now_ns();
            let start_local = ntp_fixed_to_unix_ns(start_ntp) as i128;
            let master_shift = master_now as i128 - local_now as i128;
            let wall0 = start_local + master_shift - lead_ns as i128;
            self.ptp_anchor_wall0 = Some(wall0.max(0) as u64);
            self.ptp_anchor_pos0 = self.state.timestamp;
        }
    }

    /// Arm the first native realtime timeline only after one complete PCM
    /// transport packet is buffered. This mirrors airplay-cli's cold START
    /// contract: before the first START no silence/audio is sent; the first
    /// real packet is released against one freshly frozen anchor line.
    pub fn arm_cold_start(
        &mut self,
        start_ntp: u64,
        latency_max: Option<u32>,
        lead_frames: u32,
        rtp_offset: u32,
    ) -> Result<(), MediaSendError> {
        let head_ts = ntp_to_frames(start_ntp, 44_100);
        self.state.timestamp = (head_ts as u32).wrapping_add(rtp_offset);
        self.state.first_packet = true;
        self.ptp_anchor_wall0 = None;
        self.ptp_anchor_pos0 = self.state.timestamp;
        self.splice_pad_frames = 0;
        self.configure_source_timeline(start_ntp, head_ts, latency_max, lead_frames);

        // Source announces the frozen PTP line at START, immediately before
        // the first audio release. NTP sends its first sync with first audio.
        if matches!(&self.timing, RealtimeTiming::Ptp { .. }) {
            let _ = self.prime_ptp_anchor(start_ntp, lead_frames)?;
        }
        Ok(())
    }

    fn splice_pad_to_lead(
        &mut self,
        now_ts: u64,
        lapse_ts: u64,
        lead_frames: u32,
    ) -> Option<u32> {
        let recovery_lead = (lead_frames as u64).min(self.pacing_window_frames);
        let effective_head = self.head_ts.saturating_add(self.splice_pad_frames as u64);
        if effective_head > lapse_ts {
            return None;
        }
        let target = now_ts.saturating_add(recovery_lead);
        if target <= effective_head {
            return None;
        }

        let pad = target - effective_head;
        let pad_u32 = pad.min(u32::MAX as u64) as u32;
        self.splice_pad_frames = self.splice_pad_frames.saturating_add(pad_u32);
        self.timeline_reanchors = self.timeline_reanchors.saturating_add(1);
        self.reanchor_shifted_frames = self.reanchor_shifted_frames.saturating_add(pad);
        Some(pad_u32)
    }

    pub fn recover_input_gap(&mut self, now_ntp: u64, lead_frames: u32) -> Option<u32> {
        let now_ts = ntp_to_frames(now_ntp, 44_100);
        let floor = 11_025u64; // source: AP2_MIN_WARM_LEAD_MS = 250 ms
        self.splice_pad_to_lead(
            now_ts,
            now_ts.saturating_add(floor),
            lead_frames,
        )
    }

    pub fn recover_delivery_gap(&mut self, now_ntp: u64, lead_frames: u32) -> Option<u32> {
        let now_ts = ntp_to_frames(now_ntp, 44_100);
        self.splice_pad_to_lead(now_ts, now_ts, lead_frames)
    }

    pub fn splice_pad_frames(&self) -> u32 {
        self.splice_pad_frames
    }

    /// Source-equivalent local half of a warm splice FLUSH. The receiver
    /// queue, RTP sequence, RTP timestamp and immutable anchor line stay
    /// untouched; only pad debt from the superseded content epoch is dropped.
    pub fn begin_warm_splice_boundary(&mut self) {
        self.splice_pad_frames = 0;
    }

    pub fn add_splice_pad(&mut self, frames: u32) {
        self.splice_pad_frames = self.splice_pad_frames.saturating_add(frames);
    }

    pub fn consume_splice_pad(&mut self, frames: u32) {
        self.splice_pad_frames = self.splice_pad_frames.saturating_sub(frames);
    }

    pub fn timeline_reanchors(&self) -> u64 {
        self.timeline_reanchors
    }

    pub fn reanchor_shifted_frames(&self) -> u64 {
        self.reanchor_shifted_frames
    }

    /// Signed distance from the effective delivery head to the current wall
    /// frame clock. Positive means the immutable splice line is still ahead
    /// (hot); zero/negative means the line has lapsed and any real content
    /// sent without recovery would carry a past timestamp.
    pub fn timeline_head_delta_frames(&self, now_ntp: u64) -> i64 {
        let now_ts = ntp_to_frames(now_ntp, 44_100);
        let effective_head = self
            .head_ts
            .saturating_add(self.splice_pad_frames as u64);
        let delta = effective_head as i128 - now_ts as i128;
        delta.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    pub fn can_accept_frames(&mut self, now_ntp: u64) -> bool {
        if !self.pacing_enabled {
            return true;
        }

        let now_ts = ntp_to_frames(now_ntp, 44_100);
        if now_ts.saturating_add(self.pacing_window_frames) < self.head_ts {
            return false;
        }

        let now = Instant::now();
        if let Some(last) = self.pace_last_release {
            if now.duration_since(last) < Duration::from_micros(1_000) {
                return false;
            }
        }
        self.pace_last_release = Some(now);
        true
    }

    pub fn prime_ptp_anchor(
        &mut self,
        ntp_time: u64,
        lead_frames: u32,
    ) -> Result<bool, MediaSendError> {
        if !matches!(&self.timing, RealtimeTiming::Ptp { .. }) {
            return Ok(false);
        }
        self.send_sync_packet(ntp_time, lead_frames, true)
    }

    pub fn head_ts(&self) -> u64 {
        self.head_ts
    }

    pub fn pacing_window_frames(&self) -> u64 {
        self.pacing_window_frames
    }

    pub fn state(&self) -> RtpState {
        self.state
    }

    /// Receiver PTP probe streak (Delay_Req/Pdelay_Req) observed by the timing
    /// engine. Diagnostic only: mirrors the upstream clock-readiness evidence
    /// without changing media/timeline behavior.
    pub fn uses_ptp_timing(&self) -> bool {
        matches!(self.timing, RealtimeTiming::Ptp { .. })
    }

    pub fn ptp_probe_exchange(&self) -> Option<PtpExchange> {
        match &self.timing {
            RealtimeTiming::Ptp { clock } => clock.exchange(),
            RealtimeTiming::Ntp => None,
        }
    }

    pub fn transport(&self) -> &MediaTransport {
        &self.transport
    }

    pub fn send_pcm_352(
        &mut self,
        pcm_le_stereo: &[u8],
        ntp_time: u64,
        lead_frames: u32,
    ) -> Result<MediaSendResult, MediaSendError> {
        if pcm_le_stereo.len() != ALAC_PCM_PACKET_BYTES {
            return Err(MediaSendError::Alac(if pcm_le_stereo.len() % 4 != 0 {
                AlacEncodeError::MisalignedPcm
            } else if pcm_le_stereo.len() > ALAC_PCM_PACKET_BYTES {
                AlacEncodeError::TooManyFrames
            } else {
                AlacEncodeError::Empty
            }));
        }

        let alac = encode_alac_16_stereo_352(pcm_le_stereo)?;
        self.send_alac_payload(&alac, ntp_time, lead_frames)
    }

    pub fn send_alac_payload(
        &mut self,
        alac_payload: &[u8],
        ntp_time: u64,
        lead_frames: u32,
    ) -> Result<MediaSendResult, MediaSendError> {
        let should_sync = self.state.first_packet || (!self.state.first_packet && self.state.sequence % 100 == 0);

        let sync_delivered = if should_sync {
            self.send_sync_packet(ntp_time, lead_frames, self.state.first_packet)?
        } else {
            true
        };

        let sequence_sent = self.state.sequence;
        let timestamp_sent = self.state.timestamp;
        let first_marker = self.state.first_packet;
        let packet = build_encrypted_realtime_packet(&self.state, alac_payload, &self.audio_key)?;
        let audio_delivered = match self
            .transport
            .send_data_deadline(&packet, Duration::from_millis(20))?
        {
            DatagramSendOutcome::Sent(_) => {
                if let Some(ring) = &self.retransmit {
                    ring.store(sequence_sent, &packet);
                }
                true
            }
            DatagramSendOutcome::Dropped => false,
        };

        // Upstream advances the media timeline even on a transient local UDP
        // drop; retrying an old timestamp late is worse than exposing a gap.
        // Keep the restart marker armed until both first sync and first audio
        // were accepted by the local socket.
        let clear_first = audio_delivered && sync_delivered;
        self.state
            .advance_with_marker_clear(FRAMES_PER_PACKET_44100, clear_first);
        self.head_ts = self.head_ts.wrapping_add(FRAMES_PER_PACKET_44100 as u64);

        Ok(MediaSendResult {
            sequence_sent,
            timestamp_sent,
            sync_sent: should_sync && sync_delivered,
            audio_delivered,
            first_marker,
        })
    }

    fn send_sync_packet(
        &mut self,
        ntp_time: u64,
        lead_frames: u32,
        first: bool,
    ) -> Result<bool, MediaSendError> {
        let delivered = match &self.timing {
            RealtimeTiming::Ntp => {
                let play_position = self.state.timestamp.saturating_sub(lead_frames);
                let sync = build_ntp_sync_packet(NtpSyncPacketArgs {
                    first,
                    play_position,
                    ntp_time,
                    rtp_timestamp: self.state.timestamp,
                });
                matches!(
                    self.transport
                        .send_control_deadline(&sync, Duration::from_millis(20))?,
                    DatagramSendOutcome::Sent(_)
                )
            }
            RealtimeTiming::Ptp { clock } => {
                let wall_time_ns = clock.master_now_ns();
                let clock_id = clock.master_clock_id();
                let wall0 = *self.ptp_anchor_wall0.get_or_insert(wall_time_ns);
                if first && self.ptp_anchor_wall0 == Some(wall_time_ns) {
                    self.ptp_anchor_pos0 = self.state.timestamp;
                }

                let wall_delta_ns = wall_time_ns as i128 - wall0 as i128;
                let lead_ns = frames_to_ns(lead_frames) as i128;
                let elapsed_ns = wall_delta_ns - lead_ns;
                let elapsed_frames = (elapsed_ns * 44_100i128) / 1_000_000_000i128;
                let play_pos = self.ptp_anchor_pos0.wrapping_add(elapsed_frames as u32);

                let frame_1 = play_pos.wrapping_add(11_035);
                let frame_2 = frame_1.wrapping_add(77_175);
                let sync = build_ptp_sync_packet(PtpSyncPacketArgs {
                    first,
                    frame_1,
                    wall_time_ns,
                    frame_2,
                    clock_id,
                });
                matches!(
                    self.transport
                        .send_control_deadline(&sync, Duration::from_millis(20))?,
                    DatagramSendOutcome::Sent(_)
                )
            }
        };
        Ok(delivered)
    }
}


fn system_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn frames_to_ns(frames: u32) -> u64 {
    ((frames as u128 * 1_000_000_000u128) / 44_100u128) as u64
}

fn ntp_to_frames(ntp: u64, sample_rate: u64) -> u64 {
    let sec = ntp >> 32;
    let frac = ntp & 0xFFFF_FFFF;
    sec.saturating_mul(sample_rate)
        .saturating_add(((frac as u128 * sample_rate as u128) >> 32) as u64)
}

fn ntp_fixed_to_unix_ns(ntp: u64) -> u64 {
    const NTP_UNIX_EPOCH_DELTA: u64 = 2_208_988_800;
    let sec = ntp >> 32;
    let frac = ntp & 0xFFFF_FFFF;
    let unix_sec = sec.saturating_sub(NTP_UNIX_EPOCH_DELTA);
    unix_sec
        .saturating_mul(1_000_000_000)
        .saturating_add((frac.saturating_mul(1_000_000_000)) >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaTransport, StreamPorts};
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};
    use std::time::Duration;

    fn transport_to(data_rx: &UdpSocket, ctrl_rx: &UdpSocket) -> MediaTransport {
        let mut transport = MediaTransport::bind(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        transport.attach_remote(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            StreamPorts {
                data_port: data_rx.local_addr().unwrap().port(),
                control_port: ctrl_rx.local_addr().unwrap().port(),
            },
        );
        transport
    }

    #[test]
    fn warm_boundary_keeps_wire_timeline_and_drops_only_old_pad_debt() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(0xFFFE, 0xFFFF_F000, 0);
        let mut sender = RealtimeMediaSender::new(transport, state, [0u8; 32]);
        sender.head_ts = 9_876_543_210;
        sender.splice_pad_frames = 12_345;
        sender.timeline_reanchors = 7;
        sender.reanchor_shifted_frames = 88_000;

        let before_state = sender.state();
        let before_head = sender.head_ts();
        let before_reanchors = sender.timeline_reanchors();
        let before_shift = sender.reanchor_shifted_frames();

        sender.begin_warm_splice_boundary();

        assert_eq!(sender.splice_pad_frames(), 0);
        assert_eq!(sender.state(), before_state);
        assert_eq!(sender.head_ts(), before_head);
        assert_eq!(sender.timeline_reanchors(), before_reanchors);
        assert_eq!(sender.reanchor_shifted_frames(), before_shift);
    }

    #[test]
    fn long_run_wire_timeline_survives_sequence_and_timestamp_wraps() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport = transport_to(&data_rx, &ctrl_rx);

        let initial_head = 123_456_789u64;
        let wire_offset = 0x00AB_CD00u32;
        let initial_rtp = (initial_head as u32).wrapping_add(wire_offset);
        let initial_seq = 65_000u16;
        let state = RtpState::new(initial_seq, initial_rtp, 0);
        let mut sender = RealtimeMediaSender::new(transport, state, [0u8; 32]);
        sender.head_ts = initial_head;
        sender.state.first_packet = false;

        // 12.5M packets is ~27.7 hours at 44.1 kHz / 352 fpp. This crosses
        // the 16-bit sequence space many times and the 32-bit RTP timestamp
        // at least once without touching sockets or wall time.
        const PACKETS: u64 = 12_500_000;
        for i in 0..PACKETS {
            sender.state.advance(FRAMES_PER_PACKET_44100);
            sender.head_ts = sender
                .head_ts
                .wrapping_add(FRAMES_PER_PACKET_44100 as u64);

            if i % 100_000 == 0 || i + 1 == PACKETS {
                assert_eq!(
                    sender.state.timestamp,
                    (sender.head_ts as u32).wrapping_add(wire_offset)
                );
                let sent = i + 1;
                assert_eq!(
                    sender.state.sequence,
                    initial_seq.wrapping_add(sent as u16)
                );
            }
        }

        assert!(sender.head_ts > initial_head + u32::MAX as u64);
    }

    #[test]
    fn timeline_head_delta_reports_hot_and_lapsed_lines() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(1, 1_000, 0);
        let mut sender = RealtimeMediaSender::new(transport, state, [0u8; 32]);

        let start_ntp = ((10u64) << 32);
        let head_ts = ntp_to_frames(start_ntp, 44_100);
        sender.configure_source_timeline(start_ntp, head_ts, Some(88_200), 11_025);

        assert_eq!(sender.timeline_head_delta_frames(start_ntp), 0);
        let earlier = start_ntp.saturating_sub((1u64 << 32) / 10);
        assert!(sender.timeline_head_delta_frames(earlier) > 0);
        let later = start_ntp.saturating_add((1u64 << 32) / 10);
        assert!(sender.timeline_head_delta_frames(later) < 0);
    }

    #[test]
    fn pcm_pipeline_encodes_encrypts_sends_and_advances() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(0x0102, 50_000, 0x10203040);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x55u8; 32]);

        let pcm = vec![0u8; ALAC_PCM_PACKET_BYTES];
        let result = sender.send_pcm_352(&pcm, 0x0102030405060708, 11_025).unwrap();

        assert!(result.sync_sent);
        assert_eq!(result.sequence_sent, 0x0102);
        assert_eq!(result.timestamp_sent, 50_000);

        let mut buf = [0u8; 4096];
        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(cn, 20);

        let (dn, _) = data_rx.recv_from(&mut buf).unwrap();
        assert!(dn > 12 + 16 + 8);
        assert_eq!(&buf[..12], &state.header());
        assert_eq!(sender.state().timestamp, 50_352);
    }

    #[test]
    fn ptp_sender_emits_d7_anchor_before_rtp() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(7, 100_000, 0);
        let mut sender = RealtimeMediaSender::new_ptp(
            transport,
            state,
            [0x55u8; 32],
            0xA1B2C3D4E5F60708,
        );

        let ntp = ((2_208_988_800u64 + 100) << 32);
        sender.send_alac_payload(b"x", ntp, 11_025).unwrap();

        let mut buf = [0u8; 128];
        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(cn, 28);
        assert_eq!(&buf[..4], &[0x90, 0xD7, 0x00, 0x06]);
        assert_eq!(&buf[20..28], &0xA1B2C3D4E5F60708u64.to_be_bytes());
        let _ = data_rx.recv_from(&mut buf).unwrap();
    }

    #[test]
    fn starvation_recovery_adds_only_the_missing_silence_debt() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(1, 0, 1);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x11u8; 32]);

        let now = ((2_208_988_800u64 + 10) << 32);
        let now_ts = ntp_to_frames(now, 44_100);
        sender.configure_source_timeline(now, now_ts + 5_000, Some(66_150), 11_025);

        let added = sender.recover_input_gap(now, 11_025).unwrap();
        assert_eq!(added, 6_025);
        assert_eq!(sender.splice_pad_frames(), 6_025);

        // A second call sees the effective head already recovered and must
        // not stack another shift.
        assert_eq!(sender.recover_input_gap(now, 11_025), None);
    }

    #[test]
    fn delivery_recovery_does_not_pad_a_head_still_ahead_of_now() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(1, 0, 1);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x11u8; 32]);

        let now = ((2_208_988_800u64 + 10) << 32);
        let now_ts = ntp_to_frames(now, 44_100);
        sender.configure_source_timeline(now, now_ts + 1, Some(66_150), 11_025);
        assert_eq!(sender.recover_delivery_gap(now, 11_025), None);
    }

    #[test]
    fn pcm_pipeline_rejects_partial_chunk() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(1, 0, 1);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x11u8; 32]);

        let err = sender.send_pcm_352(&vec![0u8; ALAC_PCM_PACKET_BYTES - 4], 0, 0);
        assert!(matches!(err, Err(MediaSendError::Alac(_))));
        assert_eq!(sender.state(), state);
    }

    #[test]
    fn first_send_emits_sync_then_encrypted_rtp_and_advances_352_frames() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(0x1234, 100_000, 0xAABBCCDD);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x44u8; 32]);

        let result = sender
            .send_alac_payload(b"fake-alac", 0x1122334455667788, 11_025)
            .unwrap();

        assert_eq!(
            result,
            MediaSendResult {
                sequence_sent: 0x1234,
                timestamp_sent: 100_000,
                sync_sent: true,
                audio_delivered: true,
                first_marker: true,
            }
        );

        let mut buf = [0u8; 2048];

        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(cn, 20);
        assert_eq!(&buf[..4], &[0x90, 0xD4, 0x00, 0x07]);
        assert_eq!(
            &buf[4..8],
            &100_000u32.saturating_sub(11_025).to_be_bytes()
        );
        assert_eq!(&buf[16..20], &100_000u32.to_be_bytes());

        let (dn, _) = data_rx.recv_from(&mut buf).unwrap();
        assert!(dn > 12 + 16 + 8);
        assert_eq!(&buf[..12], &state.header());

        let next = sender.state();
        assert_eq!(next.sequence, 0x1235);
        assert_eq!(next.timestamp, 100_352);
        assert!(!next.first_packet);
    }

    #[test]
    fn second_send_has_no_sync_and_marker_is_clear() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_millis(100))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(1, 0, 0x01020304);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x22u8; 32]);

        sender.send_alac_payload(b"one", 1, 0).unwrap();
        let mut buf = [0u8; 2048];
        let _ = ctrl_rx.recv_from(&mut buf).unwrap();
        let _ = data_rx.recv_from(&mut buf).unwrap();

        let second = sender.send_alac_payload(b"two", 2, 0).unwrap();
        assert!(!second.sync_sent);

        assert!(ctrl_rx.recv_from(&mut buf).is_err());

        let (dn, _) = data_rx.recv_from(&mut buf).unwrap();
        assert!(dn > 12);
        assert_eq!(buf[1], 0x60);
    }

    #[test]
    fn periodic_sync_follows_sequence_modulo_100_like_source() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        data_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        // First packet seq=98 -> initial sync, then seq=99 no sync, seq=100 sync.
        let state = RtpState::new(98, 50_000, 0x12345678);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x11u8; 32]);
        let mut buf = [0u8; 4096];

        let first = sender.send_alac_payload(b"x", 1, 0).unwrap();
        assert!(first.sync_sent);
        let _ = ctrl_rx.recv_from(&mut buf).unwrap();
        let _ = data_rx.recv_from(&mut buf).unwrap();

        let second = sender.send_alac_payload(b"x", 1, 0).unwrap();
        assert!(!second.sync_sent);
        let _ = data_rx.recv_from(&mut buf).unwrap();

        let third = sender.send_alac_payload(b"x", 1, 0).unwrap();
        assert!(third.sync_sent);
        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(cn, 20);
        assert_eq!(&buf[..2], &[0x80, 0xD4]);
        let _ = data_rx.recv_from(&mut buf).unwrap();
    }

    #[test]
    fn sync_play_position_saturates_at_zero_when_latency_exceeds_timestamp() {
        let data_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ctrl_rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        ctrl_rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();

        let transport = transport_to(&data_rx, &ctrl_rx);
        let state = RtpState::new(1, 1_000, 0x12345678);
        let mut sender = RealtimeMediaSender::new(transport, state, [0x11u8; 32]);

        sender.send_alac_payload(b"x", 1, 11_025).unwrap();

        let mut buf = [0u8; 64];
        let (cn, _) = ctrl_rx.recv_from(&mut buf).unwrap();
        assert_eq!(cn, 20);
        assert_eq!(&buf[4..8], &0u32.to_be_bytes());
    }
}
