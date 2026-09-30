#![cfg(windows)]

//! WASAPI adapter for the concrete MSA-pinned RAOP session.
//! Capture ownership mirrors the native SOLO adapter: one bounded persistent
//! PCM buffer survives PAUSE, while FLUSH advances a generation barrier and
//! discards only pre-FLUSH content.

use crate::{
    MsaRaopConfig, MsaRaopError, MsaRaopPcmWriter, MsaRaopSession,
    Pcm352Chunker, WasapiLoopbackCapture, WasapiLoopbackError,
    RAOP_PCM_PACKET_BYTES,
};
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender, TrySendError},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const RAOP_RING_CAPACITY_BYTES: usize = 1 << 20; // max(4s*176400,1MiB) = 1MiB
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
    first_start_done: Arc<AtomicBool>,
    capture_worker: Option<JoinHandle<()>>,
    writer_worker: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl WindowsRaopAudioWorker {
    pub fn connect(config:MsaRaopConfig)->Result<Self,WindowsRaopWorkerError>{
        let session=Arc::new(Mutex::new(MsaRaopSession::connect(config)?));
        Self::start(session)
    }

    pub fn start(session:SharedMsaRaopSession)->Result<Self,WindowsRaopWorkerError>{
        let pcm_writer=session.lock()
            .map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?
            .pcm_writer();

        let running=Arc::new(AtomicBool::new(true));
        let delivery_enabled=Arc::new(AtomicBool::new(false));
        let flush_generation=Arc::new(AtomicU64::new(0));
        let flush_ack_generation=Arc::new(AtomicU64::new(0));
        let first_start_done=Arc::new(AtomicBool::new(false));
        let last_error=Arc::new(Mutex::new(None));

        let (tx,rx)=mpsc::sync_channel::<(u64,[u8;RAOP_PCM_PACKET_BYTES])>(WRITER_QUEUE_PACKETS);
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

        let running_c=Arc::clone(&running);
        let enabled_c=Arc::clone(&delivery_enabled);
        let flush_c=Arc::clone(&flush_generation);
        let flush_ack_c=Arc::clone(&flush_ack_generation);
        let error_c=Arc::clone(&last_error);
        let (ready_tx,ready_rx)=mpsc::sync_channel::<Result<(),String>>(1);
        let capture_worker=thread::Builder::new().name("msa-raop-wasapi".into()).spawn(move||{
            let capture=match WasapiLoopbackCapture::open_default(){
                Ok(v)=>{let _=ready_tx.send(Ok(()));v}
                Err(e)=>{
                    let msg=e.to_string();let _=ready_tx.send(Err(msg.clone()));
                    if let Ok(mut slot)=error_c.lock(){*slot=Some(msg);}
                    running_c.store(false,Ordering::SeqCst);return;
                }
            };
            let mut chunker=Pcm352Chunker::new();
            let mut local_flush=flush_c.load(Ordering::SeqCst);
            let mut pending_packet:Option<[u8;RAOP_PCM_PACKET_BYTES]>=None;

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
                    flush_ack_c.store(generation,Ordering::SeqCst);
                }

                let report=match capture.drain_into(&mut chunker){
                    Ok(v)=>v,
                    Err(e)=>{
                        if let Ok(mut slot)=error_c.lock(){*slot=Some(e.to_string());}
                        running_c.store(false,Ordering::SeqCst);break;
                    }
                };
                let _=chunker.truncate_pending(RAOP_RING_CAPACITY_BYTES);

                if !enabled_c.load(Ordering::SeqCst) {
                    if report.frames==0 { thread::sleep(Duration::from_millis(1)); }
                    continue;
                }

                loop {
                    if pending_packet.is_none() {
                        let Some(packet)=chunker.pop_packet() else { break };
                        pending_packet=Some(packet.try_into().expect("RAOP chunker is fixed PCM16 stereo"));
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
                first_start_done,capture_worker:Some(capture_worker),
                writer_worker:Some(writer_worker),last_error,
            }),
            Ok(Err(message))=>{
                running.store(false,Ordering::SeqCst);
                let _=capture_worker.join();let _=writer_worker.join();
                Err(WindowsRaopWorkerError::Worker(message))
            }
            Err(mpsc::RecvTimeoutError::Disconnected)=>{
                running.store(false,Ordering::SeqCst);
                let _=capture_worker.join();let _=writer_worker.join();
                Err(WindowsRaopWorkerError::Worker("RAOP WASAPI worker disconnected before ready".into()))
            }
            Err(mpsc::RecvTimeoutError::Timeout)=>{
                running.store(false,Ordering::SeqCst);
                let _=capture_worker.join();let _=writer_worker.join();
                Err(WindowsRaopWorkerError::Worker("RAOP WASAPI worker did not become ready".into()))
            }
        }
    }

    pub fn session(&self)->SharedMsaRaopSession{Arc::clone(&self.session)}
    pub fn is_running(&self)->bool{self.running.load(Ordering::SeqCst)}
    pub fn last_error(&self)->Option<String>{self.last_error.lock().ok().and_then(|v|v.clone())}

    pub fn commit_start(&self,requested_unix_ms:u64)->Result<crate::timing::StartResolution,WindowsRaopWorkerError>{
        let mut session=self.session.lock().map_err(|_|WindowsRaopWorkerError::Worker("RAOP session mutex poisoned".into()))?;
        let start=if self.first_start_done.load(Ordering::SeqCst) {
            session.start_after_flush(requested_unix_ms)?
        } else {
            session.commit_start(requested_unix_ms)?
        };
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
        if let Ok(mut session)=self.session.lock(){session.disconnect();}
    }
}
impl Drop for WindowsRaopAudioWorker{fn drop(&mut self){self.stop();}}
