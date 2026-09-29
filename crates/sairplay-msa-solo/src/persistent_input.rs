//! Persistent PCM input ownership contract from pinned MSA ap2_session.c.
//!
//! This module models the single reader that survives track boundaries. The
//! platform adapter owns the actual nonblocking descriptor/read primitive;
//! the engine owns pause/drain/refill ordering so Windows cannot redesign it.

use crate::pcm_ring::PcmRing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderState { Running, DrainRequested, Paused, Aborted }

#[derive(Debug,PartialEq,Eq)]
pub enum ReaderError<E>{InvalidConfig,InvalidState,Input(E)}

pub trait PersistentInput {
    type Error;
    /// Nonblocking read. None means would-block; Some(empty) means EOF.
    fn read_nonblocking(&mut self, dst:&mut [u8])->Result<Option<usize>,Self::Error>;
}

pub struct PersistentReader<I:PersistentInput> {
    input:I,
    ring:PcmRing,
    ready_bytes:usize,
    audio_seen:bool,
    state:ReaderState,
}

impl<I:PersistentInput> PersistentReader<I> {
    pub fn new(input:I,byte_rate:usize,ready_bytes:usize)->Result<Self,ReaderError<I::Error>>{
        if byte_rate==0 || ready_bytes==0{return Err(ReaderError::InvalidConfig);}
        let ring=PcmRing::for_byte_rate(byte_rate);
        if ready_bytes>ring.capacity(){return Err(ReaderError::InvalidConfig);}
        Ok(Self{input,ring,ready_bytes,audio_seen:false,state:ReaderState::Running})
    }
    pub fn state(&self)->ReaderState{self.state}
    pub fn buffered_bytes(&self)->usize{self.ring.fill()}
    pub fn writable_bytes(&self)->usize{self.ring.capacity()-self.ring.fill()}
    pub fn audio_ready(&self)->bool{self.audio_seen}
    pub fn eof(&self)->bool{self.ring.eof()}
    pub fn read_playing(&mut self,out:&mut[u8],playing:bool)->i32{
        if self.state==ReaderState::Aborted{return -2;}
        if !playing || out.is_empty(){return 0;}
        if self.ring.fill()>=out.len() || (self.ring.eof() && self.ring.fill()>0){return self.ring.pop(out) as i32;}
        if self.ring.eof(){return -1;}
        0
    }
    pub fn discard_playing(&mut self,want:usize,playing:bool)->i32{
        if self.state==ReaderState::Aborted{return -2;}
        if !playing || want==0{return 0;}
        if self.ring.fill()>0{return self.ring.discard(want) as i32;}
        if self.ring.eof(){return -1;}
        0
    }
    pub fn request_drain(&mut self){if self.state!=ReaderState::Aborted{self.state=ReaderState::DrainRequested;}}
    /// Reader-side acknowledgement. MSA parks before the command thread may
    /// touch the same input descriptor.
    pub fn acknowledge_pause(&mut self)->bool{
        if self.state==ReaderState::DrainRequested{self.state=ReaderState::Paused;true}else{false}
    }
    /// Exclusive command-side drain: valid only after reader pause ack.
    pub fn drain_preflush(&mut self)->Result<usize,ReaderError<I::Error>>{
        if self.state!=ReaderState::Paused{return Err(ReaderError::InvalidState);}
        self.ring.reset();
        self.audio_seen=false;
        let mut total=0usize;
        let mut scratch=[0u8;16384];
        for _ in 0..100_000 {
            match self.input.read_nonblocking(&mut scratch).map_err(ReaderError::Input)? {
                None=>break,
                Some(0)=>break,
                Some(n)=>total=total.saturating_add(n),
            }
        }
        Ok(total)
    }
    pub fn resume_after_drain(&mut self)->Result<(),ReaderError<I::Error>>{
        if self.state!=ReaderState::Paused{return Err(ReaderError::InvalidState);}
        self.state=ReaderState::Running;Ok(())
    }
    pub fn abort(&mut self){self.state=ReaderState::Aborted;}
    /// One persistent reader step. No read is allowed while drain is pending.
    pub fn pump_once(&mut self)->Result<usize,I::Error>{
        if self.state!=ReaderState::Running || self.ring.eof(){return Ok(0);}
        let free=self.writable_bytes();
        if free==0{return Ok(0);}
        let mut buf=[0u8;16384];
        let want=free.min(buf.len());
        match self.input.read_nonblocking(&mut buf[..want])? {
            None=>Ok(0),
            Some(0)=>{self.ring.mark_eof();if !self.audio_seen && self.ring.fill()>0{self.audio_seen=true;}Ok(0)}
            Some(n)=>{
                let pushed=self.ring.push(&buf[..n]);
                if !self.audio_seen && self.ring.fill()>=self.ready_bytes{self.audio_seen=true;}
                Ok(pushed)
            }
        }
    }
    pub fn pop(&mut self,out:&mut[u8])->usize{self.ring.pop(out)}
}

#[cfg(test)]
mod tests{
 use super::*;
 struct Input{chunks:Vec<Vec<u8>>,i:usize}
 impl PersistentInput for Input{
  type Error=();
  fn read_nonblocking(&mut self,d:&mut[u8])->Result<Option<usize>,Self::Error>{
   if self.i>=self.chunks.len(){return Ok(None)}
   let c=&self.chunks[self.i];self.i+=1;
   if c.is_empty(){return Ok(Some(0))}
   let n=c.len().min(d.len());d[..n].copy_from_slice(&c[..n]);Ok(Some(n))
  }
 }
 #[test] fn invalid_config_is_rejected_without_panic(){
  let i=Input{chunks:vec![],i:0};assert!(matches!(PersistentReader::new(i,0,1),Err(ReaderError::InvalidConfig)));
  let i=Input{chunks:vec![],i:0};assert!(matches!(PersistentReader::new(i,100,0),Err(ReaderError::InvalidConfig)));
 }
 #[test] fn ready_is_one_shot_until_flush_drain(){
  let i=Input{chunks:vec![vec![1,2,3,4],vec![9,9],vec![7,7,7,7]],i:0};
  let mut r=PersistentReader::new(i,100,4).unwrap();
  assert_eq!(r.pump_once().unwrap(),4);assert!(r.audio_ready());
  r.request_drain();assert!(r.acknowledge_pause());
  assert_eq!(r.drain_preflush().unwrap(),6);assert_eq!(r.buffered_bytes(),0);assert!(!r.audio_ready());
  r.resume_after_drain().unwrap();assert_eq!(r.state(),ReaderState::Running);
 }
 #[test] fn full_ring_applies_backpressure_without_consuming_input(){
  struct Count{reads:usize}
  impl PersistentInput for Count{type Error=();fn read_nonblocking(&mut self,d:&mut[u8])->Result<Option<usize>,Self::Error>{self.reads+=1;d.fill(1);Ok(Some(d.len()))}}
  let i=Count{reads:0};let mut r=PersistentReader::new(i,1,1).unwrap();
  while r.writable_bytes()>0{r.pump_once().unwrap();}
  let before=r.input.reads;assert_eq!(r.pump_once().unwrap(),0);assert_eq!(r.input.reads,before);
 }
 #[test] fn read_and_discard_match_msa_playing_and_eof_semantics(){
  let i=Input{chunks:vec![vec![1,2,3],vec![]],i:0};let mut r=PersistentReader::new(i,100,1).unwrap();
  r.pump_once().unwrap();let mut out=[0u8;4];assert_eq!(r.read_playing(&mut out,true),0);
  r.pump_once().unwrap();assert_eq!(r.read_playing(&mut out,true),3);assert_eq!(&out[..3],&[1,2,3]);
  assert_eq!(r.discard_playing(1,true),-1);assert_eq!(r.read_playing(&mut out,false),0);
 }
 #[test] fn reader_never_touches_input_during_drain_handshake(){
  let i=Input{chunks:vec![vec![1]],i:0};let mut r=PersistentReader::new(i,100,1).unwrap();
  r.request_drain();assert_eq!(r.pump_once().unwrap(),0);assert!(r.acknowledge_pause());
 }
}
