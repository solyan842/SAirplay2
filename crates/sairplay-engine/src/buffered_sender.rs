use crate::{build_encrypted_buffered_frame, AudioPacketError, RtpState, FRAMES_PER_PACKET_44100};
use std::io::{self, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum BufferedSendError {
    Packet(AudioPacketError),
    Io(io::Error),
    PendingFrame,
}

impl From<AudioPacketError> for BufferedSendError {
    fn from(value: AudioPacketError) -> Self { Self::Packet(value) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferedWriteOutcome {
    Sent,
    Backpressured,
}

pub struct BufferedMediaSender<W: Write = TcpStream> {
    writer: W,
    state: RtpState,
    audio_key: [u8; 32],
    nonce_counter: u64,
    pending: Vec<u8>,
    pending_offset: usize,
    anchored: bool,
}

impl<W: Write> BufferedMediaSender<W> {
    pub fn new(writer: W, state: RtpState, audio_key: [u8; 32]) -> Self {
        Self {
            writer,
            state,
            audio_key,
            nonce_counter: 0,
            pending: Vec::new(),
            pending_offset: 0,
            anchored: false,
        }
    }

    pub fn state(&self) -> RtpState { self.state }
    pub fn nonce_counter(&self) -> u64 { self.nonce_counter }
    pub fn pending_bytes(&self) -> usize {
        self.pending.len().saturating_sub(self.pending_offset)
    }

    pub fn pending_offset(&self) -> usize { self.pending_offset }
    pub fn is_anchored(&self) -> bool { self.anchored }
    pub fn mark_anchored(&mut self) { self.anchored = true; }
    pub fn clear_anchored(&mut self) { self.anchored = false; }

    /// Source-aligned pre-FLUSHBUFFERED quiesce for the portable/Windows path.
    /// If none of a parked frame reached the socket yet, drop it completely.
    /// If a partial frame was written, finish that frame if possible so the
    /// TCP framing cannot be truncated. The source gives this at most 1 s.
    /// Windows has no kernel send-queue introspection in the pinned code, so
    /// after the parked tail is handled there is no additional drain wait.
    pub fn quiesce_for_flush(&mut self) -> Result<bool, BufferedSendError> {
        if self.pending.is_empty() {
            return Ok(true);
        }

        if self.pending_offset == 0 {
            self.pending.clear();
            self.pending_offset = 0;
            return Ok(true);
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match self.flush_pending()? {
                BufferedWriteOutcome::Sent => return Ok(true),
                BufferedWriteOutcome::Backpressured => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// Retry the unwritten tail. TimedOut/WouldBlock are flow control, not a
    /// session failure, matching pinned MSA's buffered_pending behavior.
    pub fn flush_pending(&mut self) -> Result<BufferedWriteOutcome, BufferedSendError> {
        while self.pending_offset < self.pending.len() {
            match self.writer.write(&self.pending[self.pending_offset..]) {
                Ok(0) => return Ok(BufferedWriteOutcome::Backpressured),
                Ok(written) => self.pending_offset += written,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Ok(BufferedWriteOutcome::Backpressured);
                }
                Err(error) => return Err(BufferedSendError::Io(error)),
            }
        }

        self.pending.clear();
        self.pending_offset = 0;
        Ok(BufferedWriteOutcome::Sent)
    }

    /// Commit one already-encoded ALAC packet to the buffered TCP stream.
    /// A previous pending tail must clear first; callers should use
    /// flush_pending() as their accept gate before submitting another frame.
    pub fn send_alac_payload(
        &mut self,
        alac_payload: &[u8],
    ) -> Result<BufferedWriteOutcome, BufferedSendError> {
        if self.pending_bytes() != 0 {
            return Err(BufferedSendError::PendingFrame);
        }

        let frame = build_encrypted_buffered_frame(
            &self.state,
            alac_payload,
            &self.audio_key,
            self.nonce_counter,
        )?;

        // Encryption consumed this nonce and the frame is committed in-order
        // even if only part reaches the kernel during this call.
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        self.pending = frame;
        self.pending_offset = 0;

        let outcome = self.flush_pending()?;
        self.state.advance(FRAMES_PER_PACKET_44100);
        Ok(outcome)
    }

    pub fn into_inner(self) -> W { self.writer }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ChunkWriter {
        max_once: usize,
        bytes: Vec<u8>,
        block_after_first: bool,
        writes: usize,
    }

    impl Write for ChunkWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.block_after_first && self.writes > 1 {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "full"));
            }
            let n = buf.len().min(self.max_once.max(1));
            self.bytes.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn flush_quiesce_drops_wholly_unwritten_parked_frame() {
        let writer = ChunkWriter {
            max_once: 8,
            block_after_first: true,
            ..Default::default()
        };
        let state = RtpState::new(1, 2000, 0);
        let mut sender = BufferedMediaSender::new(writer, state, [0x44; 32]);

        // Build a parked frame explicitly at offset zero, mirroring a frame
        // accepted into the stash before any kernel write.
        sender.pending = vec![0, 4, 0x80, 0x67];
        sender.pending_offset = 0;
        assert!(sender.quiesce_for_flush().unwrap());
        assert_eq!(sender.pending_bytes(), 0);
        assert_eq!(sender.pending_offset(), 0);
    }

    #[test]
    fn anchor_state_can_be_cleared_by_flush_path() {
        let writer = ChunkWriter { max_once: usize::MAX, ..Default::default() };
        let state = RtpState::new(1, 2000, 0);
        let mut sender = BufferedMediaSender::new(writer, state, [0x44; 32]);
        assert!(!sender.is_anchored());
        sender.mark_anchored();
        assert!(sender.is_anchored());
        sender.clear_anchored();
        assert!(!sender.is_anchored());
    }

    #[test]
    fn full_write_advances_rtp_and_nonce_once() {
        let writer = ChunkWriter { max_once: usize::MAX, ..Default::default() };
        let state = RtpState::new(7, 1000, 0);
        let mut sender = BufferedMediaSender::new(writer, state, [0x33; 32]);

        assert_eq!(
            sender.send_alac_payload(b"alac").unwrap(),
            BufferedWriteOutcome::Sent
        );
        assert_eq!(sender.nonce_counter(), 1);
        assert_eq!(sender.state().sequence, 8);
        assert_eq!(sender.state().timestamp, 1352);
        assert!(!sender.state().first_packet);
        assert_eq!(sender.pending_bytes(), 0);

        let writer = sender.into_inner();
        assert_eq!(u16::from_be_bytes([writer.bytes[0], writer.bytes[1]]) as usize, writer.bytes.len());
        assert_eq!(writer.bytes[3], 0xE7);
    }

    #[test]
    fn partial_write_parks_tail_and_rejects_new_frame() {
        let writer = ChunkWriter {
            max_once: 8,
            block_after_first: true,
            ..Default::default()
        };
        let state = RtpState::new(1, 2000, 0);
        let mut sender = BufferedMediaSender::new(writer, state, [0x44; 32]);

        assert_eq!(
            sender.send_alac_payload(b"encoded-frame").unwrap(),
            BufferedWriteOutcome::Backpressured
        );
        assert!(sender.pending_bytes() > 0);
        assert!(matches!(
            sender.send_alac_payload(b"next"),
            Err(BufferedSendError::PendingFrame)
        ));
        assert_eq!(sender.nonce_counter(), 1);
        assert_eq!(sender.state().sequence, 2);
        assert_eq!(sender.state().timestamp, 2352);
    }
}
