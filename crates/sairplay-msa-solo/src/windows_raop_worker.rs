#![cfg(windows)]

//! WASAPI adapter for the concrete MSA-pinned RAOP session.
//! Capture ownership mirrors the native SOLO adapter: one bounded persistent
//! PCM buffer survives PAUSE, while FLUSH advances a generation barrier and
//! discards only pre-FLUSH content.

use crate::{
    Ap2AudioFormat, MsaRaopConfig, MsaRaopError, MsaRaopPcmWriter, MsaRaopSession,
    Pcm352Chunker, WasapiLoopbackCapture, WasapiLoopbackError,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender, TrySendError},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const WRITER_QUEUE_PACKETS: usize = 256;
const FLUSH_ACK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum WindowsRaopWorkerError {
    Session(MsaRaopError),
    Capture(WasapiLoopbackError),
    Worker(String),
}
impl fmt::Display for WindowsRaopWorkerError {
    fn fmt(&self, f:&mut fmt::Formatter<'_>)->fmt::Result {
        match self {
            Self::Session(e)=>write!(f,"{e}"),
            Self::Capture(e)=>write!(f,"{e}"),
            Self::Worker(e)=>write!(f,"{e}"),
        }
    }
}
impl std::error::Error for WindowsRaopWorkerError {}
impl From<MsaRaopError> for WindowsRaopWorkerError { fn from(v:MsaRaopError)->Self{Self::Session(v)} }
impl From<WasapiLoopbackError> for WindowsRaopWorkerError { fn from(v:WasapiLoopbackError)->Self{Self::Capture(v)} }

pub type SharedMsaRaopSession = Arc<Mutex<MsaRaopSession>>;

pub struct WindowsRaopAudioWorker {
    session: SharedMsaRaopSession,
    running: Arc<AtomicBool>,
    delivery_enabled: Arc<AtomicBool>,
    flush_generation: Arc<AtomicU64>,
    flush_ack_generation: Arc<AtomicU64>,
    audio_ready: Arc<AtomicBool>,
    first_start_done: Arc<AtomicBool>,
    capture_worker: Option<JoinHandle<()>>,
    writer_worker: Option<JoinHandle<()>>,
    health_worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl WindowsRaopAudioWorker {
    pub fn connect(config:MsaRaopConfig)->Result<Self,WindowsRaopWorkerError>{
        let session=Arc::new(Mutex::new(MsaRaopSession::connect(config)?));
        Self::start(session)
    }

    pub fn start(session:SharedMsaRaopSession)->Result<Self,WindowsRaopWorkerError>{
        let (pcm_writer, ready)={
            let guard=session.lock()
                .map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            (guard.pcm_writer(), guard.ready())
        };
        let audio_format=Ap2AudioFormat {
            sample_rate: ready.sample_rate,
            bit_depth: ready.bit_depth,
            channels: ready.channels,
        };
        let input_bpf=audio_format.input_bytes_per_frame();
        let byte_rate=audio_format.sample_rate as usize * input_bpf;
        let ring_capacity_bytes=(byte_rate.saturating_mul(4)).max(1 << 20);

        let running=Arc::new(AtomicBool::new(true));
        let delivery_enabled=Arc::new(AtomicBool::new(false));
        let flush_generation=Arc::new(AtomicU64::new(0));
        let flush_ack_generation=Arc::new(AtomicU64::new(0));
        let audio_ready=Arc::new(AtomicBool::new(false));
        let first_start_done=Arc::new(AtomicBool::new(false));
        let last_error=Arc::new(Mutex::new(None));

        let (tx,rx)=mpsc::sync_channel::<(u64,Vec<u8>)>(WRITER_QUEUE_PACKETS);
        let running_w=Arc::clone(&running);
        let flush_w=Arc::clone(&flush_generation);
        let error_w=Arc::clone(&last_error);
        let writer_worker=thread::Builder::new().name("msa-raop-writer".into()).spawn(move||{
            while running_w.load(Ordering::SeqCst) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok((generation,packet))=>{
                        if generation != flush_w.load(Ordering::SeqCst) {
                            continue;
                        }
                        if let Err(e)=pcm_writer.write_packet(&packet) {
                            if let Ok(mut slot)=error_w.lock(){*slot=Some(e.to_string());}
                            running_w.store(false,Ordering::SeqCst);
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout)=>{}
                    Err(mpsc::RecvTimeoutError::Disconnected)=>break,
                }
            }
        }).map_err(|e|WindowsRaopWorkerError::Worker(format!("spawn RAOP writer: {e}")))?;

        let running_h=Arc::clone(&running);
        let session_h=Arc::clone(&session);
        let error_h=Arc::clone(&last_error);
        let health_worker=thread::Builder::new().name("msa-raop-health".into()).spawn(move||{
            while running_h.load(Ordering::SeqCst) {
                let alive=session_h.lock().map(|mut s|s.helper_alive()).unwrap_or(false);
                if !alive {
                    if let Ok(mut slot)=error_h.lock(){*slot=Some("RAOP helper exited (control/media unhealthy)".into());}
                    running_h.store(false,Ordering::SeqCst);
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }).map_err(|e|WindowsRaopWorkerError::Worker(format!("spawn RAOP health monitor: {e}")))?;

        let running_c=Arc::clone(&running);
        let enabled_c=Arc::clone(&delivery_enabled);
        let flush_c=Arc::clone(&flush_generation);
        let flush_ack_c=Arc::clone(&flush_ack_generation);
        let audio_ready_c=Arc::clone(&audio_ready);
        let error_c=Arc::clone(&last_error);
        let (ready_tx,ready_rx)=mpsc::sync_channel::<Result<(),String>>(1);
        let capture_worker=thread::Builder::new().name("msa-raop-wasapi".into()).spawn(move||{
            let capture=match WasapiLoopbackCapture::open_default_for_format(audio_format){
                Ok(v)=>{let _=ready_tx.send(Ok(()));v}
                Err(e)=>{
                    let msg=e.to_string();let _=ready_tx.send(Err(msg.clone()));
                    if let Ok(mut slot)=error_c.lock(){*slot=Some(msg);}
                    running_c.store(false,Ordering::SeqCst);return;
                }
            };
            let mut chunker=Pcm352Chunker::new_with_bytes_per_frame(input_bpf);
            let mut local_flush=flush_c.load(Ordering::SeqCst);
            let mut pending_packet:Option<Vec<u8>>=None;

            while running_c.load(Ordering::SeqCst) {
                let generation=flush_c.load(Ordering::SeqCst);
                if generation!=local_flush {
                    // The transport FLUSH has already completed while control
                    // held its own serialization. Reset only bytes retained
                    // before that boundary; subsequent WASAPI data becomes the
                    // next persistent-session content.
                    chunker.clear();
                    pending_packet=None;
                    local_flush=generation;
                    audio_ready_c.store(false,Ordering::SeqCst);
                    flush_ack_c.store(generation,Ordering::SeqCst);
                }

                let report=match capture.drain_into(&mut chunker){
                    Ok(v)=>v,
                    Err(e)=>{
                        if let Ok(mut slot)=error_c.lock(){*slot=Some(e.to_string());}
                        running_c.store(false,Ordering::SeqCst);break;
                    }
                };
                let _=chunker.truncate_pending(ring_capacity_bytes);
                if chunker.has_packet() {
                    audio_ready_c.store(true, Ordering::SeqCst);
                }

                if !enabled_c.load(Ordering::SeqCst) {
                    if report.frames==0 { thread::sleep(Duration::from_millis(1)); }
                    continue;
                }

                loop {
                    if pending_packet.is_none() {
                        let Some(packet)=chunker.pop_packet() else { break };
                        pending_packet=Some(packet);
                    }
                    let packet=pending_packet.take().unwrap();
                    match tx.try_send((local_flush,packet)) {
                        Ok(())=>{}
                        Err(TrySendError::Full((_generation,packet)))=>{
                            pending_packet=Some(packet);
                            break;
                        }
                        Err(TrySendError::Disconnected(_))=>{
                            running_c.store(false,Ordering::SeqCst);
                            break;
                        }
                    }
                }
                if report.frames==0 { thread::sleep(Duration::from_millis(1)); }
            }
        }).map_err(|e|WindowsRaopWorkerError::Worker(format!("spawn RAOP capture: {e}")))?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(()))=>Ok(Self{
                session,running,delivery_enabled,flush_generation,flush_ack_generation,
                audio_ready,first_start_done,capture_worker:Some(capture_worker),
                writer_worker:Some(writer_worker),health_worker:Some(health_worker),last_error,
            }),
            Ok(Err(message))=>{
                running.store(false,Ordering::SeqCst);
                let _=capture_worker.join();let _=writer_worker.join() ;
                Err(WindowsRaopWorkerError::Worker(message))
            }
            Err(mpsc::RecvTimeoutError::Disconnected)=>{
                running.store(false,Ordering::SeqCst);
                let _=capture_worker.join();let _=writer_worker.join();let _=health_worker.join();
                Err(WindowsRaopWorkerError::Worker("RAOP WASAPI worker disconnected before ready".into()))
            }
            Err(mpsc::RecvTimeoutError::Timeout)=>{
                running.store(false,Ordering::SeqCst);
                let _=capture_worker.join();let _=writer_worker.join();let _=health_worker.join();
                Err(WindowsRaopWorkerError::Worker("RAOP WASAPI worker did not become ready".into()))
            }
        }
    }

    pub fn session(&self)->SharedMsaRaopSession{Arc::clone(&self.session)}
    pub fn audio_ready(&self)->bool{self.audio_ready.load(Ordering::SeqCst)}
    pub fn is_running(&self)->bool{self.running.load(Ordering::SeqCst)}
    pub fn last_error(&self)->Option<String>{self.last_error.lock().ok().and_then(|v|v.clone())}

    pub fn commit_start(&self,requested_unix_ms:u64)->Result<crate::timing::StartResolution,WindowsRaopWorkerError>{
        let mut session=self.session.lock().map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        let first = !self.first_start_done.load(Ordering::SeqCst);
        let start=if first {
            session.commit_start(requested_unix_ms)?
        } else {
            session.start_after_flush(requested_unix_ms)?
        };
        if first {
            // Same gate as cliairplay session_commit: metadata must land
            // after the START commit but before captured PCM delivery opens.
            session.ensure_initial_metadata()?;
        }
        self.first_start_done.store(true,Ordering::SeqCst);
        self.delivery_enabled.store(true,Ordering::SeqCst);
        Ok(start)
    }

    pub fn flush_content(&self)->Result<(),WindowsRaopWorkerError>{
        {
            let mut session=self.session.lock().map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            session.flush()?;
        }
        self.delivery_enabled.store(false,Ordering::SeqCst);
        let generation=self.flush_generation.fetch_add(1,Ordering::SeqCst)+1;
        let deadline=Instant::now()+FLUSH_ACK_TIMEOUT;
        while self.flush_ack_generation.load(Ordering::SeqCst)<generation {
            if !self.is_running(){return Err(WindowsRaopWorkerError::Worker("RAOP worker stopped during FLUSH".into()));}
            if Instant::now()>=deadline{return Err(WindowsRaopWorkerError::Worker("RAOP FLUSH PCM barrier timed out".into()));}
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    pub fn standby_content(&self)->Result<(),WindowsRaopWorkerError>{
        {
            let mut session=self.session.lock().map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            session.standby()?;
        }
        self.delivery_enabled.store(false,Ordering::SeqCst);
        Ok(())
    }

    pub fn pause_content(&self)->Result<(),WindowsRaopWorkerError>{
        {
            let mut session=self.session.lock().map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
            session.pause()?;
        }
        // No flush-generation bump: new captured content remains in the
        // bounded persistent ring for ACTION=PLAY.
        self.delivery_enabled.store(false,Ordering::SeqCst);
        Ok(())
    }

    pub fn play_content(&self)->Result<(),WindowsRaopWorkerError>{
        let mut session=self.session.lock().map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.play()?;
        self.delivery_enabled.store(true,Ordering::SeqCst);
        Ok(())
    }

    pub fn stop_content(&self)->Result<(),WindowsRaopWorkerError>{
        let mut session=self.session.lock().map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        session.stop()?;
        self.delivery_enabled.store(false,Ordering::SeqCst);
        Ok(())
    }

    pub fn stop(&mut self){
        self.running.store(false,Ordering::SeqCst);
        self.delivery_enabled.store(false,Ordering::SeqCst);
        if let Some(w)=self.capture_worker.take(){let _=w.join();}
        if let Some(w)=self.writer_worker.take(){let _=w.join();}
        if let Some(w)=self.health_worker.take(){let _=w.join();}
        if let Ok(mut session)=self.session.lock(){session.disconnect();}
    }
}
impl Drop for WindowsRaopAudioWorker{fn drop(&mut self){self.stop();}}
