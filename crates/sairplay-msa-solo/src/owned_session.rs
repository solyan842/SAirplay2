//! End-to-end MSA SOLO ownership: session commands + persistent reader.
//! The platform input primitive is injected; lifecycle/order remains engine-owned.

use crate::{SessionState, StartAck};
use crate::persistent_input::{PersistentInput, PersistentReader, ReaderState};

pub trait OwnedTransport {
    type Error;
    fn quiesce(&mut self);
    fn flush(&mut self)->Result<(),Self::Error>;
    fn commit_start(&mut self,requested_unix_ms:u64)->Result<u64,Self::Error>;
    fn stop(&mut self);
    fn warm_head_unix_ms(&self)->u64 { 0 }
    fn resume(&mut self);
    fn disconnect(&mut self);
}

#[derive(Debug,PartialEq,Eq)]
pub enum OwnedError<TE,IE>{Ended,Transport(TE),Input(IE)}

pub struct OwnedSoloSession<T:OwnedTransport,I:PersistentInput>{
    transport:T,
    reader:PersistentReader<I>,
    state:SessionState,
    epoch:u64,
}

impl<T:OwnedTransport,I:PersistentInput> OwnedSoloSession<T,I>{
    pub fn new(transport:T,input:I,byte_rate:usize,ready_bytes:usize)->Self{
        Self{transport,reader:PersistentReader::new(input,byte_rate,ready_bytes),state:SessionState::Idle,epoch:0}
    }
    pub fn state(&self)->SessionState{self.state}
    pub fn epoch(&self)->u64{self.epoch}
    pub fn audio_ready(&self)->bool{self.reader.audio_ready()}
    pub fn pump_input_once(&mut self)->Result<usize,I::Error>{self.reader.pump_once()}

    pub fn start(&mut self,requested:u64)->Result<StartAck,OwnedError<T::Error,I::Error>>{
        if self.state==SessionState::Ended{return Err(OwnedError::Ended);}
        self.transport.quiesce();
        let r=self.transport.commit_start(requested);
        self.transport.resume();
        let at=r.map_err(OwnedError::Transport)?;
        self.epoch=self.epoch.wrapping_add(1);
        self.state=SessionState::Playing;
        Ok(StartAck{requested_unix_ms:requested,at_unix_ms:at})
    }

    /// Exact MSA command order: quiesce -> transport FLUSH -> park reader ->
    /// reset ring/drain old input -> IDLE -> capture warm head -> resume.
    pub fn flush(&mut self)->Result<u64,OwnedError<T::Error,I::Error>>{
        if self.state==SessionState::Ended{return Err(OwnedError::Ended);}
        self.transport.quiesce();
        if let Err(e)=self.transport.flush(){self.transport.resume();return Err(OwnedError::Transport(e));}
        self.reader.request_drain();
        assert!(self.reader.acknowledge_pause() || self.reader.state()==ReaderState::Paused);
        if let Err(e)=self.reader.drain_preflush(){self.reader.resume_after_drain();self.transport.resume();return Err(OwnedError::Input(e));}
        self.state=SessionState::Idle;
        let head=self.transport.warm_head_unix_ms();
        self.reader.resume_after_drain();
        self.transport.resume();
        Ok(head)
    }

    pub fn standby(&mut self)->Result<(),OwnedError<T::Error,I::Error>>{
        if self.state==SessionState::Ended{return Err(OwnedError::Ended);}
        self.transport.quiesce();self.transport.stop();self.state=SessionState::Standby;self.transport.resume();Ok(())
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
  fn stop(&mut self){self.log.push("stop")} fn warm_head_unix_ms(&self)->u64{777}
  fn resume(&mut self){self.log.push("resume")} fn disconnect(&mut self){self.log.push("disconnect")}
 }
 struct I{chunks:Vec<Vec<u8>>,i:usize}
 impl PersistentInput for I{type Error=();fn read_nonblocking(&mut self,d:&mut[u8])->Result<Option<usize>,Self::Error>{
  if self.i>=self.chunks.len(){return Ok(None)} let c=&self.chunks[self.i];self.i+=1;let n=c.len();d[..n].copy_from_slice(c);Ok(Some(n))
 }}
 #[test] fn flush_is_end_to_end_msa_order_and_rearms_reader(){
  let i=I{chunks:vec![vec![1,2,3,4],vec![8,8]],i:0};let mut s=OwnedSoloSession::new(T::default(),i,100,4);
  assert_eq!(s.pump_input_once().unwrap(),4);assert!(s.audio_ready());
  s.start(1000).unwrap();let head=s.flush().unwrap();assert_eq!(head,777);assert_eq!(s.state(),SessionState::Idle);assert!(!s.audio_ready());
  assert_eq!(s.transport.log,vec!["quiesce","commit","resume","quiesce","flush","resume"]);
 }
}
