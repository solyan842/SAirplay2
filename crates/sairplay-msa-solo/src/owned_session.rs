//! End-to-end MSA SOLO ownership: session commands + persistent reader.
//! The platform input primitive is injected; lifecycle/order remains engine-owned.

use crate::{SessionState, StartAck};
use crate::persistent_input::{PersistentInput, PersistentReader, ReaderError, ReaderState};

pub trait OwnedTransport {
    type Error;
    fn quiesce(&mut self);
    fn flush(&mut self)->Result<(),Self::Error>;
    fn commit_start(&mut self,requested_unix_ms:u64)->Result<u64,Self::Error>;
    fn stop(&mut self);
    fn warm_head_unix_ms(&self)->Option<u64> { None }
    fn resume(&mut self);
    fn disconnect(&mut self);
}

#[derive(Debug,PartialEq,Eq)]
pub enum OwnedError<TE,IE>{Ended,Transport(TE),Input(IE),ReaderState}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum SessionEvent{AudioReady{buffered_ms:u64},Flushed{head_unix_ms:Option<u64>},IdleTimeout}

pub struct OwnedSoloSession<T:OwnedTransport,I:PersistentInput>{
    transport:T,
    reader:PersistentReader<I>,
    state:SessionState,
    epoch:u64,
    byte_rate:usize,
    idle_timeout_ms:u64,
    idle_since_ms:u64,
    last_audio_ready:bool,
    pending_event:Option<SessionEvent>,
}

impl<T:OwnedTransport,I:PersistentInput> OwnedSoloSession<T,I>{
    pub fn new(transport:T,input:I,byte_rate:usize,ready_bytes:usize)->Self{Self::new_at(transport,input,byte_rate,ready_bytes,0,0)}
    pub fn new_at(transport:T,input:I,byte_rate:usize,ready_bytes:usize,idle_timeout_ms:u64,now_ms:u64)->Self{
        Self{transport,reader:PersistentReader::new(input,byte_rate,ready_bytes),state:SessionState::Idle,epoch:0,byte_rate,idle_timeout_ms,idle_since_ms:now_ms,last_audio_ready:false,pending_event:None}
    }
    pub fn state(&self)->SessionState{self.state}
    pub fn epoch(&self)->u64{self.epoch}
    pub fn audio_ready(&self)->bool{self.reader.audio_ready()}
    pub fn pump_input_once(&mut self)->Result<usize,I::Error>{self.reader.pump_once()}
    /// Timed reader pump used by the owned runtime: MSA opens the orphan
    /// timeout window at the exact reader transition to EOF.
    pub fn pump_input_once_at(&mut self,now_ms:u64)->Result<usize,I::Error>{
        let was_eof=self.reader.eof();
        let n=self.reader.pump_once()?;
        if !was_eof && self.reader.eof(){self.idle_since_ms=now_ms;}
        Ok(n)
    }
    pub fn reader_state(&self)->ReaderState{self.reader.state()}
    /// Reader-side park acknowledgement; the runtime reader loop calls this,
    /// never the command-side FLUSH path.
    pub fn reader_acknowledge_pause(&mut self)->bool{self.reader.acknowledge_pause()}
    pub fn read(&mut self,out:&mut[u8])->i32{self.reader.read_playing(out,self.state==SessionState::Playing)}
    pub fn discard(&mut self,want:usize)->i32{self.reader.discard_playing(want,self.state==SessionState::Playing)}
    pub fn take_event(&mut self)->Option<SessionEvent>{
        if let Some(e)=self.pending_event.take(){return Some(e);}
        let ready=self.reader.audio_ready();if ready && !self.last_audio_ready{self.last_audio_ready=true;return Some(SessionEvent::AudioReady{buffered_ms:self.reader.buffered_bytes() as u64*1000/self.byte_rate as u64});}None
    }
    pub fn poll_at(&mut self,now_ms:u64)->Option<SessionEvent>{
        if self.idle_timeout_ms==0 || self.state==SessionState::Ended{return None;}
        let idle=self.state==SessionState::Idle || self.state==SessionState::Standby || (self.state==SessionState::Playing && self.reader.eof());
        if idle && now_ms.saturating_sub(self.idle_since_ms)>=self.idle_timeout_ms{self.end();return Some(SessionEvent::IdleTimeout);}None
    }

    pub fn start_at(&mut self,requested:u64,now_ms:u64)->Result<StartAck,OwnedError<T::Error,I::Error>>{
        if self.state==SessionState::Ended{return Err(OwnedError::Ended);}
        self.transport.quiesce();
        let r=self.transport.commit_start(requested);
        self.transport.resume();
        let at=r.map_err(OwnedError::Transport)?;
        self.epoch=self.epoch.wrapping_add(1);
        self.state=SessionState::Playing;
        self.idle_since_ms=now_ms;
        Ok(StartAck{requested_unix_ms:requested,at_unix_ms:at})
    }

    /// Command-side phase 1: quiesce/FLUSH then request the persistent reader
    /// to park. Completion is illegal until the reader loop acknowledges.
    pub fn begin_flush(&mut self)->Result<(),OwnedError<T::Error,I::Error>>{
        if self.state==SessionState::Ended{return Err(OwnedError::Ended);}
        self.transport.quiesce();
        if let Err(e)=self.transport.flush(){self.transport.resume();return Err(OwnedError::Transport(e));}
        self.reader.request_drain();
        Ok(())
    }

    /// Command-side phase 2, called only after reader-side pause acknowledgement.
    pub fn complete_flush_at(&mut self,now_ms:u64)->Result<Option<u64>,OwnedError<T::Error,I::Error>>{
        if self.state==SessionState::Ended{return Err(OwnedError::Ended);}
        if self.reader.state()!=ReaderState::Paused{return Err(OwnedError::ReaderState);}
        if let Err(e)=self.reader.drain_preflush(){let _=self.reader.resume_after_drain();self.transport.resume();return Err(match e{ReaderError::Input(e)=>OwnedError::Input(e),ReaderError::InvalidState=>OwnedError::ReaderState});}
        self.state=SessionState::Idle;
        self.idle_since_ms=now_ms;
        self.last_audio_ready=false;
        let head=self.transport.warm_head_unix_ms();
        self.reader.resume_after_drain().map_err(|e|match e{ReaderError::Input(e)=>OwnedError::Input(e),ReaderError::InvalidState=>OwnedError::ReaderState})?;
        self.transport.resume();
        self.pending_event=Some(SessionEvent::Flushed{head_unix_ms:head});
        Ok(head)
    }

    pub fn standby_at(&mut self,now_ms:u64)->Result<(),OwnedError<T::Error,I::Error>>{
        if self.state==SessionState::Ended{return Err(OwnedError::Ended);}
        self.transport.quiesce();self.transport.stop();self.state=SessionState::Standby;self.idle_since_ms=now_ms;self.transport.resume();Ok(())
    }
    /// MSA END marks ENDED and wakes/stops the reader; teardown is outer lifecycle.
    pub fn end(&mut self){self.reader.abort();self.state=SessionState::Ended;}
    pub fn destroy(mut self){if self.state!=SessionState::Ended{self.end();}self.transport.disconnect();}
}

#[cfg(test)]
mod tests{
 use super::*;
 #[derive(Default)] struct T{log:Vec<&'static str>}
 impl OwnedTransport for T{
  type Error=();
  fn quiesce(&mut self){self.log.push("quiesce")} fn flush(&mut self)->Result<(),Self::Error>{self.log.push("flush");Ok(())}
  fn commit_start(&mut self,r:u64)->Result<u64,Self::Error>{self.log.push("commit");Ok(r)}
  fn stop(&mut self){self.log.push("stop")} fn warm_head_unix_ms(&self)->Option<u64>{Some(777)}
  fn resume(&mut self){self.log.push("resume")} fn disconnect(&mut self){self.log.push("disconnect")}
 }
 struct I{chunks:Vec<Vec<u8>>,i:usize}
 impl PersistentInput for I{type Error=();fn read_nonblocking(&mut self,d:&mut[u8])->Result<Option<usize>,Self::Error>{
  if self.i>=self.chunks.len(){return Ok(None)} let c=&self.chunks[self.i];self.i+=1;let n=c.len();d[..n].copy_from_slice(c);Ok(Some(n))
 }}
 #[test] fn flush_is_end_to_end_msa_order_and_rearms_reader(){
  let i=I{chunks:vec![vec![1,2,3,4],vec![8,8]],i:0};let mut s=OwnedSoloSession::new(T::default(),i,100,4);
  assert_eq!(s.pump_input_once().unwrap(),4);assert!(s.audio_ready());
  s.start_at(1000,10).unwrap();s.begin_flush().unwrap();assert_eq!(s.reader_state(),ReaderState::DrainRequested);assert!(s.reader_acknowledge_pause());let head=s.complete_flush_at(20).unwrap();assert_eq!(head,Some(777));assert_eq!(s.state(),SessionState::Idle);assert!(!s.audio_ready());
  assert_eq!(s.transport.log,vec!["quiesce","commit","resume","quiesce","flush","resume"]);
  assert_eq!(s.take_event(),Some(SessionEvent::Flushed{head_unix_ms:Some(777)}));
 }
 #[test] fn flush_completion_requires_reader_ack(){
  let i=I{chunks:vec![],i:0};let mut s=OwnedSoloSession::new(T::default(),i,100,4);
  s.begin_flush().unwrap();assert_eq!(s.complete_flush_at(20),Err(OwnedError::ReaderState));
  assert!(s.reader_acknowledge_pause());assert_eq!(s.complete_flush_at(20).unwrap(),Some(777));
 }
 #[test] fn flushed_status_can_omit_absent_warm_head(){
  #[derive(Default)] struct NoHead;
  impl OwnedTransport for NoHead{type Error=();fn quiesce(&mut self){}fn flush(&mut self)->Result<(),Self::Error>{Ok(())}fn commit_start(&mut self,r:u64)->Result<u64,Self::Error>{Ok(r)}fn stop(&mut self){}fn resume(&mut self){}fn disconnect(&mut self){}}
  let i=I{chunks:vec![],i:0};let mut s=OwnedSoloSession::new(NoHead,i,100,4);
  s.begin_flush().unwrap();assert!(s.reader_acknowledge_pause());assert_eq!(s.complete_flush_at(20).unwrap(),None);
  assert_eq!(s.take_event(),Some(SessionEvent::Flushed{head_unix_ms:None}));
 }
 #[test] fn idle_timeout_tracks_start_and_eof_window(){
  let i=I{chunks:vec![vec![]],i:0};let mut s=OwnedSoloSession::new_at(T::default(),i,100,4,100,5);
  s.start_at(1000,50).unwrap();s.pump_input_once_at(80).unwrap();assert_eq!(s.poll_at(179),None);
  assert_eq!(s.poll_at(180),Some(SessionEvent::IdleTimeout));assert_eq!(s.state(),SessionState::Ended);
 }
}
