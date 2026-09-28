//! Single-owner Windows runtime session for the SAirplay 2.0 MSA clone.
//!
//! This composes the three previously separate pieces around one shared
//! MsaSessionCore:
//! - WASAPI producer -> MsaWindowsSource
//! - lifecycle START/FLUSH/STANDBY -> one MsaSessionTransport
//! - consumer -> source packets only while PLAYING
//!
//! Transport commit happens first; only after it succeeds is the source state
//! changed. This preserves the MSA ordering and prevents Windows capture/sender
//! code from publishing playback state on its own.

use crate::{
    MsaAp2VerifyEvent, MsaAp2VerifyResult, MsaFlushAck, MsaSessionState,
    MsaSessionTransport, MsaStartAck, MsaWindowsCaptureError,
    MsaWindowsCaptureWorker, MsaWindowsConsumerWorker, MsaWindowsPacketSink,
    MsaWindowsSource,
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsaPendingStart {
    pub requested_unix_ms: u64,
    pub transport_committed_unix_ms: u64,
}

pub struct MsaWindowsRuntimeSession<T: MsaSessionTransport> {
    source: Arc<Mutex<MsaWindowsSource>>,
    transport: T,
    capture: Option<MsaWindowsCaptureWorker>,
    consumer: Option<MsaWindowsConsumerWorker>,
    pending_start: Option<MsaPendingStart>,
}

impl<T: MsaSessionTransport> MsaWindowsRuntimeSession<T> {
    pub fn new(source: MsaWindowsSource, transport: T) -> Self {
        Self {
            source: Arc::new(Mutex::new(source)),
            transport,
            capture: None,
            consumer: None,
            pending_start: None,
        }
    }

    pub fn source(&self) -> &Arc<Mutex<MsaWindowsSource>> {
        &self.source
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn start_capture(&mut self) -> Result<(), MsaWindowsCaptureError> {
        if self.capture.is_none() {
            self.capture = Some(MsaWindowsCaptureWorker::start(Arc::clone(&self.source))?);
        }
        Ok(())
    }

    pub fn start_consumer(&mut self, sink: Box<dyn MsaWindowsPacketSink>) {
        if self.consumer.is_none() {
            self.consumer = Some(MsaWindowsConsumerWorker::start(
                Arc::clone(&self.source),
                sink,
            ));
        }
    }

    pub fn state(&self) -> Result<MsaSessionState, String> {
        self.source
            .lock()
            .map(|source| source.state())
            .map_err(|_| "MSA Windows source lock poisoned".to_owned())
    }

    pub fn audio_present(&self) -> Result<bool, String> {
        self.source
            .lock()
            .map(|source| source.audio_present())
            .map_err(|_| "MSA Windows source lock poisoned".to_owned())
    }

    fn quiesce_consumer(&self) -> Result<(), String> {
        if let Some(consumer) = self.consumer.as_ref() {
            consumer.quiesce()?;
        }
        Ok(())
    }

    fn resume_consumer(&self) {
        if let Some(consumer) = self.consumer.as_ref() {
            consumer.resume();
        }
    }

    fn quiesce_lifecycle(&mut self) -> Result<(), String> {
        self.quiesce_consumer()?;
        if let Err(error) = self.transport.quiesce() {
            self.resume_consumer();
            return Err(error);
        }
        Ok(())
    }

    fn resume_lifecycle(&mut self) -> Result<(), String> {
        let transport_result = self.transport.resume();
        self.resume_consumer();
        transport_result
    }

    pub fn start(&mut self, requested_start_unix_ms: u64) -> Result<MsaStartAck, String> {
        if self.pending_start.is_some() {
            return Err("cannot START while an MSA deferred START is pending".into());
        }

        self.quiesce_lifecycle()?;
        let committed = match self.transport.commit_start(requested_start_unix_ms) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.resume_lifecycle();
                return Err(error);
            }
        };

        let epoch_result = {
            let mut source = self
                .source
                .lock()
                .map_err(|_| "MSA Windows source lock poisoned".to_owned())?;
            source.core_mut().start_committed()
        };
        let epoch = match epoch_result {
            Ok(value) => value,
            Err(error) => {
                let _ = self.resume_lifecycle();
                return Err(error);
            }
        };

        self.resume_lifecycle()?;
        Ok(MsaStartAck {
            requested_unix_ms: requested_start_unix_ms,
            committed_unix_ms: committed,
            epoch,
        })
    }

    /// Commit the transport anchor while deliberately keeping the PCM session
    /// IDLE. This is the native AP2 cold-join shape where the START ack is
    /// withheld until receiver-clock verification answers. Because the source
    /// never enters PLAYING here, the consumer cannot drain or send real PCM.
    pub fn begin_deferred_start(
        &mut self,
        requested_start_unix_ms: u64,
    ) -> Result<MsaPendingStart, String> {
        if self.pending_start.is_some() {
            return Err("MSA deferred START already pending".into());
        }
        if self.state()? == MsaSessionState::Ended {
            return Err("cannot START an ended MSA Windows session".into());
        }

        self.quiesce_lifecycle()?;
        let committed = match self.transport.commit_start(requested_start_unix_ms) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.resume_lifecycle();
                return Err(error);
            }
        };
        self.resume_lifecycle()?;

        let pending = MsaPendingStart {
            requested_unix_ms: requested_start_unix_ms,
            transport_committed_unix_ms: committed,
        };
        self.pending_start = Some(pending);
        Ok(pending)
    }

    /// Publish PLAYING only after cold-clock verification has produced the
    /// audible instant that actually stands. A corrected join therefore
    /// releases PCM against the corrected ACK truth, not the original guess.
    pub fn complete_deferred_start(
        &mut self,
        acknowledged_unix_ms: u64,
    ) -> Result<MsaStartAck, String> {
        let pending = self
            .pending_start
            .take()
            .ok_or_else(|| "no MSA deferred START is pending".to_owned())?;

        let epoch = self
            .source
            .lock()
            .map_err(|_| "MSA Windows source lock poisoned".to_owned())?
            .core_mut()
            .start_committed()?;

        Ok(MsaStartAck {
            requested_unix_ms: pending.requested_unix_ms,
            committed_unix_ms: acknowledged_unix_ms,
            epoch,
        })
    }

    /// Cancel a deferred START when a superseding FLUSH/STANDBY/END wins.
    /// The source has remained IDLE throughout, so no rollback of consumed PCM
    /// is necessary.
    pub fn cancel_deferred_start(&mut self) -> Option<MsaPendingStart> {
        self.pending_start.take()
    }

    pub fn pending_start(&self) -> Option<MsaPendingStart> {
        self.pending_start
    }

    /// Consume the terminal AP2 clock-verification event for a deferred cold
    /// join START. The AP2 backend owns any required anchor rebase; this layer
    /// only publishes PLAYING once the backend emits the final START ack truth.
    ///
    /// Idle means verification is still pending. Every terminal event for a
    /// deferred join must carry start_ack=true exactly once.
    pub fn apply_deferred_ap2_verification(
        &mut self,
        event: MsaAp2VerifyEvent,
    ) -> Result<Option<MsaStartAck>, String> {
        if event.result == MsaAp2VerifyResult::Idle {
            return Ok(None);
        }

        if self.pending_start.is_none() {
            return Err("AP2 verification completed without a pending deferred START".into());
        }

        if !event.start_ack {
            return Err(
                "terminal AP2 verification for deferred START omitted START ack".into(),
            );
        }

        // Verified and Unverified both keep the original anchor in event.at;
        // Corrected carries the rebased audible instant. In every case the
        // event's at_unix_ms is the only truth released to the session owner.
        self.complete_deferred_start(event.at_unix_ms).map(Some)
    }

    pub fn flush(&mut self) -> Result<MsaFlushAck, String> {
        self.cancel_deferred_start();
        self.quiesce_lifecycle()?;
        let warm_head = match self.transport.flush() {
            Ok(value) => value,
            Err(error) => {
                let _ = self.resume_lifecycle();
                return Err(error);
            }
        };

        let flush_result = {
            let mut source = self
                .source
                .lock()
                .map_err(|_| "MSA Windows source lock poisoned".to_owned())?;
            source.core_mut().flush_committed()
        };
        if let Err(error) = flush_result {
            let _ = self.resume_lifecycle();
            return Err(error);
        }

        self.resume_lifecycle()?;
        Ok(MsaFlushAck {
            warm_head_unix_ms: warm_head,
        })
    }

    pub fn standby(&mut self) -> Result<(), String> {
        self.cancel_deferred_start();
        self.quiesce_lifecycle()?;
        if let Err(error) = self.transport.stop() {
            let _ = self.resume_lifecycle();
            return Err(error);
        }

        let standby_result = {
            let mut source = self
                .source
                .lock()
                .map_err(|_| "MSA Windows source lock poisoned".to_owned())?;
            source.core_mut().standby_committed()
        };
        if let Err(error) = standby_result {
            let _ = self.resume_lifecycle();
            return Err(error);
        }

        self.resume_lifecycle()
    }

    pub fn end(&mut self) -> Result<(), String> {
        self.cancel_deferred_start();
        if let Some(mut consumer) = self.consumer.take() {
            consumer.stop();
        }
        if let Some(mut capture) = self.capture.take() {
            capture.stop();
        }
        self.source
            .lock()
            .map_err(|_| "MSA Windows source lock poisoned".to_owned())?
            .core_mut()
            .end();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WINDOWS_PCM_PACKET_BYTES_16_441_STEREO;

    #[derive(Default)]
    struct FakeTransport {
        calls: Vec<&'static str>,
        committed: u64,
        fail_start: bool,
        fail_flush: bool,
    }

    impl MsaSessionTransport for FakeTransport {
        fn quiesce(&mut self) -> Result<(), String> {
            self.calls.push("quiesce");
            Ok(())
        }

        fn commit_start(&mut self, requested_start_unix_ms: u64) -> Result<u64, String> {
            self.calls.push("start");
            if self.fail_start {
                Err("start failed".into())
            } else {
                Ok(if self.committed == 0 {
                    requested_start_unix_ms
                } else {
                    self.committed
                })
            }
        }

        fn flush(&mut self) -> Result<Option<u64>, String> {
            self.calls.push("flush");
            if self.fail_flush {
                Err("flush failed".into())
            } else {
                Ok(Some(50_000))
            }
        }

        fn stop(&mut self) -> Result<(), String> {
            self.calls.push("stop");
            Ok(())
        }

        fn resume(&mut self) -> Result<(), String> {
            self.calls.push("resume");
            Ok(())
        }
    }

    fn runtime(transport: FakeTransport) -> MsaWindowsRuntimeSession<FakeTransport> {
        MsaWindowsRuntimeSession::new(
            MsaWindowsSource::new(
                44_100 * 2 * 2,
                WINDOWS_PCM_PACKET_BYTES_16_441_STEREO,
            ).unwrap(),
            transport,
        )
    }

    #[test]
    fn transport_start_commits_before_source_enters_playing() {
        let mut rt = runtime(FakeTransport {
            committed: 12_345,
            ..Default::default()
        });
        {
            let mut source = rt.source.lock().unwrap();
            source.push_capture_pcm(
                &vec![1u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO],
            );
        }

        let ack = rt.start(12_000).unwrap();

        assert_eq!(ack.committed_unix_ms, 12_345);
        assert_eq!(rt.state().unwrap(), MsaSessionState::Playing);
        assert_eq!(rt.transport.calls, vec!["quiesce", "start", "resume"]);
    }

    #[test]
    fn failed_transport_start_never_publishes_playing() {
        let mut rt = runtime(FakeTransport {
            fail_start: true,
            ..Default::default()
        });

        assert!(rt.start(12_000).is_err());
        assert_eq!(rt.state().unwrap(), MsaSessionState::Idle);
        assert_eq!(rt.transport.calls, vec!["quiesce", "start", "resume"]);
    }

    #[test]
    fn flush_transport_happens_before_source_boundary() {
        let mut rt = runtime(FakeTransport::default());
        {
            let mut source = rt.source.lock().unwrap();
            source.push_capture_pcm(
                &vec![2u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO],
            );
        }
        rt.start(10_000).unwrap();
        let ack = rt.flush().unwrap();

        assert_eq!(ack.warm_head_unix_ms, Some(50_000));
        assert_eq!(rt.state().unwrap(), MsaSessionState::Idle);
        assert_eq!(rt.source.lock().unwrap().buffered_bytes(), 0);
        assert_eq!(
            rt.transport.calls,
            vec!["quiesce", "start", "resume", "quiesce", "flush", "resume"]
        );
    }

    #[test]
    fn failed_flush_preserves_source_state_and_buffer() {
        let mut rt = runtime(FakeTransport {
            fail_flush: true,
            ..Default::default()
        });
        {
            let mut source = rt.source.lock().unwrap();
            source.push_capture_pcm(
                &vec![3u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO],
            );
        }
        rt.start(10_000).unwrap();
        let before = rt.source.lock().unwrap().buffered_bytes();

        assert!(rt.flush().is_err());
        assert_eq!(rt.state().unwrap(), MsaSessionState::Playing);
        assert_eq!(rt.source.lock().unwrap().buffered_bytes(), before);
    }

    #[test]
    fn standby_keeps_buffer_but_stops_consumption_state() {
        let mut rt = runtime(FakeTransport::default());
        {
            let mut source = rt.source.lock().unwrap();
            source.push_capture_pcm(
                &vec![4u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO],
            );
        }
        rt.start(10_000).unwrap();
        let before = rt.source.lock().unwrap().buffered_bytes();

        rt.standby().unwrap();

        assert_eq!(rt.state().unwrap(), MsaSessionState::Standby);
        assert_eq!(rt.source.lock().unwrap().buffered_bytes(), before);
        assert_eq!(
            rt.transport.calls,
            vec!["quiesce", "start", "resume", "quiesce", "stop", "resume"]
        );
    }

    #[test]
    fn deferred_start_keeps_source_idle_until_verified_ack() {
        let mut rt = runtime(FakeTransport {
            committed: 12_345,
            ..Default::default()
        });
        {
            let mut source = rt.source.lock().unwrap();
            source.push_capture_pcm(
                &vec![8u8; WINDOWS_PCM_PACKET_BYTES_16_441_STEREO],
            );
        }

        let pending = rt.begin_deferred_start(12_000).unwrap();

        assert_eq!(pending.transport_committed_unix_ms, 12_345);
        assert_eq!(rt.state().unwrap(), MsaSessionState::Idle);
        assert_eq!(
            rt.source.lock().unwrap().buffered_bytes(),
            WINDOWS_PCM_PACKET_BYTES_16_441_STEREO
        );

        let ack = rt.complete_deferred_start(12_600).unwrap();
        assert_eq!(ack.requested_unix_ms, 12_000);
        assert_eq!(ack.committed_unix_ms, 12_600);
        assert_eq!(ack.epoch, 1);
        assert_eq!(rt.state().unwrap(), MsaSessionState::Playing);
    }

    #[test]
    fn flush_supersedes_pending_start_without_publishing_playing() {
        let mut rt = runtime(FakeTransport {
            committed: 12_345,
            ..Default::default()
        });
        rt.begin_deferred_start(12_000).unwrap();

        let ack = rt.flush().unwrap();

        assert_eq!(ack.warm_head_unix_ms, Some(50_000));
        assert!(rt.pending_start().is_none());
        assert_eq!(rt.state().unwrap(), MsaSessionState::Idle);
        assert_eq!(rt.source.lock().unwrap().core().epoch(), 0);
    }

    #[test]
    fn second_start_is_rejected_while_deferred_start_is_pending() {
        let mut rt = runtime(FakeTransport {
            committed: 12_345,
            ..Default::default()
        });
        rt.begin_deferred_start(12_000).unwrap();

        assert!(rt.start(13_000).is_err());
        assert_eq!(rt.state().unwrap(), MsaSessionState::Idle);
        assert_eq!(rt.source.lock().unwrap().core().epoch(), 0);
    }


    #[test]
    fn idle_verification_keeps_deferred_start_blocked() {
        let mut rt = runtime(FakeTransport {
            committed: 12_345,
            ..Default::default()
        });
        rt.begin_deferred_start(12_000).unwrap();

        let result = rt.apply_deferred_ap2_verification(MsaAp2VerifyEvent {
            result: MsaAp2VerifyResult::Idle,
            requested_unix_ms: 0,
            from_unix_ms: 0,
            at_unix_ms: 0,
            margin_ms: 0,
            content_cut_ms: 0,
            start_ack: false,
        }).unwrap();

        assert!(result.is_none());
        assert!(rt.pending_start().is_some());
        assert_eq!(rt.state().unwrap(), MsaSessionState::Idle);
        assert_eq!(rt.source.lock().unwrap().core().epoch(), 0);
    }

    #[test]
    fn corrected_verification_ack_releases_playing_at_corrected_truth() {
        let mut rt = runtime(FakeTransport {
            committed: 12_345,
            ..Default::default()
        });
        rt.begin_deferred_start(12_000).unwrap();

        let ack = rt.apply_deferred_ap2_verification(MsaAp2VerifyEvent {
            result: MsaAp2VerifyResult::Corrected,
            requested_unix_ms: 12_000,
            from_unix_ms: 12_345,
            at_unix_ms: 12_700,
            margin_ms: -355,
            content_cut_ms: 0,
            start_ack: true,
        }).unwrap().unwrap();

        assert_eq!(ack.requested_unix_ms, 12_000);
        assert_eq!(ack.committed_unix_ms, 12_700);
        assert_eq!(ack.epoch, 1);
        assert!(rt.pending_start().is_none());
        assert_eq!(rt.state().unwrap(), MsaSessionState::Playing);
    }

    #[test]
    fn verified_or_unverified_terminal_ack_can_release_original_anchor() {
        for result in [MsaAp2VerifyResult::Verified, MsaAp2VerifyResult::Unverified] {
            let mut rt = runtime(FakeTransport {
                committed: 12_345,
                ..Default::default()
            });
            rt.begin_deferred_start(12_000).unwrap();

            let ack = rt.apply_deferred_ap2_verification(MsaAp2VerifyEvent {
                result,
                requested_unix_ms: 12_000,
                from_unix_ms: 12_345,
                at_unix_ms: 12_345,
                margin_ms: 0,
                content_cut_ms: 0,
                start_ack: true,
            }).unwrap().unwrap();

            assert_eq!(ack.committed_unix_ms, 12_345);
            assert_eq!(rt.state().unwrap(), MsaSessionState::Playing);
        }
    }

    #[test]
    fn terminal_deferred_verification_without_ack_is_rejected() {
        let mut rt = runtime(FakeTransport {
            committed: 12_345,
            ..Default::default()
        });
        rt.begin_deferred_start(12_000).unwrap();

        assert!(rt.apply_deferred_ap2_verification(MsaAp2VerifyEvent {
            result: MsaAp2VerifyResult::Corrected,
            requested_unix_ms: 12_000,
            from_unix_ms: 12_345,
            at_unix_ms: 12_700,
            margin_ms: -355,
            content_cut_ms: 355,
            start_ack: false,
        }).is_err());

        assert!(rt.pending_start().is_some());
        assert_eq!(rt.state().unwrap(), MsaSessionState::Idle);
        assert_eq!(rt.source.lock().unwrap().core().epoch(), 0);
    }

}
