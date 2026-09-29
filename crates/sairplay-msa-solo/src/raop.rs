//! RAOP session lane, source-aligned with MSA raop_session.c.

use crate::timing::{resolve_raop_start, StartResolution};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaopState { Streaming, Flushed, Other }

pub trait RaopClient {
    type Error;
    fn state(&self) -> RaopState;
    fn now_unix_ms(&self) -> u64;
    fn latency_frames(&self) -> u32;
    fn sample_rate(&self) -> u32;
    fn stop(&mut self);
    fn flush(&mut self) -> Result<(), Self::Error>;
    fn pause(&mut self);
    fn start_at_audible_unix_ms(&mut self, audible_unix_ms: u64) -> Result<(), Self::Error>;
}

pub fn commit<C: RaopClient>(client: &mut C, requested_unix_ms: u64) -> Result<StartResolution, C::Error> {
    let state=client.state();
    assert!(matches!(state,RaopState::Streaming|RaopState::Flushed),"MSA RAOP commit requires STREAMING or FLUSHED");
    let start=resolve_raop_start(client.now_unix_ms(),requested_unix_ms);
    client.stop();
    if state==RaopState::Streaming { client.flush()?; }
    client.start_at_audible_unix_ms(start.at_unix_ms)?;
    Ok(start)
}

pub fn start_after_flush<C: RaopClient>(client:&mut C,requested_unix_ms:u64)->Result<StartResolution,C::Error>{
    assert_eq!(client.state(),RaopState::Flushed,"MSA RAOP warm START requires FLUSHED");
    let start=resolve_raop_start(client.now_unix_ms(),requested_unix_ms);
    client.start_at_audible_unix_ms(start.at_unix_ms)?;
    Ok(start)
}

pub fn flush<C:RaopClient>(client:&mut C)->Result<(),C::Error>{
    let state=client.state();
    assert!(matches!(state,RaopState::Streaming|RaopState::Flushed),"MSA RAOP FLUSH invalid state");
    client.stop();
    if state==RaopState::Streaming { client.flush()?; }
    Ok(())
}

pub fn standby<C:RaopClient>(client:&mut C)->Result<(),C::Error>{ flush(client) }

pub fn pause<C:RaopClient>(client:&mut C)->Result<(),C::Error>{
    match client.state() {
        RaopState::Flushed=>Ok(()),
        RaopState::Streaming=>{client.pause();client.flush()},
        RaopState::Other=>panic!("MSA RAOP PAUSE invalid state"),
    }
}

pub fn resume<C:RaopClient>(client:&mut C)->Result<StartResolution,C::Error>{
    assert!(matches!(client.state(),RaopState::Flushed|RaopState::Streaming),"MSA RAOP RESUME invalid state");
    let start=resolve_raop_start(client.now_unix_ms(),0);
    client.start_at_audible_unix_ms(start.at_unix_ms)?;
    Ok(start)
}

pub fn next_head_unix_ms(playtime_unix_ms:u64,chunk_frames:u32,sample_rate:u32)->u64{
    if playtime_unix_ms==0 || sample_rate==0 { return 0; }
    playtime_unix_ms + (u64::from(chunk_frames)*1000/u64::from(sample_rate))
}

#[cfg(test)]
mod tests {
 use super::*;
 #[derive(Default)] struct Fake{state:u8,now:u64,stops:u8,flushes:u8,starts:Vec<u64>}
 impl RaopClient for Fake{
  type Error=();
  fn state(&self)->RaopState{match self.state{1=>RaopState::Streaming,2=>RaopState::Flushed,_=>RaopState::Other}}
  fn now_unix_ms(&self)->u64{self.now} fn latency_frames(&self)->u32{0} fn sample_rate(&self)->u32{44100}
  fn stop(&mut self){self.stops+=1} fn flush(&mut self)->Result<(),Self::Error>{self.flushes+=1;self.state=2;Ok(())}
  fn pause(&mut self){} fn start_at_audible_unix_ms(&mut self,t:u64)->Result<(),Self::Error>{self.starts.push(t);self.state=1;Ok(())}
 }
 #[test] fn live_commit_discards_backlog_and_schedules_true_start(){let mut f=Fake{state:1,now:1000,..Default::default()};let a=commit(&mut f,1100).unwrap();assert_eq!(a.at_unix_ms,1400);assert_eq!((f.stops,f.flushes),(1,1));assert_eq!(f.starts,vec![1400]);}
 #[test] fn warm_start_requires_flushed_and_does_not_flush_again(){let mut f=Fake{state:2,now:1000,..Default::default()};let a=start_after_flush(&mut f,1500).unwrap();assert_eq!(a.at_unix_ms,1500);assert_eq!(f.flushes,0);}
 #[test] fn head_projection_is_contiguous(){assert_eq!(next_head_unix_ms(1000,441,44100),1010);}
}
