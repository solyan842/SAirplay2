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
    MsaFlushAck, MsaSessionState, MsaSessionTransport, MsaStartAck,
    MsaWindowsCaptureError, MsaWindowsCaptureWorker, MsaWindowsConsumerWorker,
    MsaWindowsPacketSink, MsaWindowsSource,
};
use std::sync::{Arc, Mutex};

pub struct MsaWindowsRuntimeSession<T: MsaSessionTransport> {
    source: Arc<Mutex<MsaWindowsSource>>,
    transport: T,
    capture: Option<MsaWindowsCaptureWorker>,
    consumer: Option<MsaWindowsConsumerWorker>,
}

impl<T: MsaSessionTransport> MsaWindowsRuntimeSession<T> {
    pub fn new(source: MsaWindowsSource, transport: T) -> Self {
        Self {
            source: Arc::new(Mutex::new(source)),
            transport,
            capture: None,
            consumer: None,
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

    pub fn start(&mut self, requested_start_unix_ms: u64) -> Result<MsaStartAck, String> {
        self.transport.quiesce()?;
        let committed = match self.transport.commit_start(requested_start_unix_ms) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.transport.resume();
                return Err(error);
            }
        };

        let epoch = match self.source.lock() {
            Ok(mut source) => source.core_mut().start_committed()?,
            Err(_) => {
                let _ = self.transport.resume();
                return Err("MSA Windows source lock poisoned".into());
            }
        };

        self.transport.resume()?;
        Ok(MsaStartAck {
            requested_unix_ms: requested_start_unix_ms,
            committed_unix_ms: committed,
            epoch,
        })
    }

    pub fn flush(&mut self) -> Result<MsaFlushAck, String> {
        self.transport.quiesce()?;
        let warm_head = match self.transport.flush() {
            Ok(value) => value,
            Err(error) => {
                let _ = self.transport.resume();
                return Err(error);
            }
        };

        match self.source.lock() {
            Ok(mut source) => source.core_mut().flush_committed()?,
            Err(_) => {
                let _ = self.transport.resume();
                return Err("MSA Windows source lock poisoned".into());
            }
        }

        self.transport.resume()?;
        Ok(MsaFlushAck {
            warm_head_unix_ms: warm_head,
        })
    }

    pub fn standby(&mut self) -> Result<(), String> {
        self.transport.quiesce()?;
        if let Err(error) = self.transport.stop() {
            let _ = self.transport.resume();
            return Err(error);
        }

        match self.source.lock() {
            Ok(mut source) => source.core_mut().standby_committed()?,
            Err(_) => {
                let _ = self.transport.resume();
                return Err("MSA Windows source lock poisoned".into());
            }
        }

        self.transport.resume()
    }

    pub fn end(&mut self) -> Result<(), String> {
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
}
