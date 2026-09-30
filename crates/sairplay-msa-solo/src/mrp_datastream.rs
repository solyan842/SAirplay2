use crate::{EncryptedRtspChannel, EncryptedRtspError, MrpController, RtspRequest, SharedCseq};
use chacha20poly1305::{aead::{Aead, Payload}, ChaCha20Poly1305, KeyInit, Key, Nonce};
use hkdf::Hkdf;
use plist::{Dictionary, Value};
use rand::RngCore;
use sha2::Sha512;
use std::io::{Read, Write, Cursor};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};


pub const MRP_STREAM_TYPE_REMOTE_CONTROL: u64 = 130;
pub const MRP_STREAM_CONTROL_TYPE: u64 = 2;
pub const MRP_CLIENT_TYPE_UUID: &str = "1910A70F-DBC0-4242-AF95-115DB30604E1";
const FRAME_MAX: usize = 1024;
const TAG_SIZE: usize = 16;
const DATA_HDR_LEN: usize = 32;

#[derive(Debug)]
pub enum MrpDataStreamError {
    Plist(plist::Error),
    Transport(EncryptedRtspError),
    Status(u16),
    MissingDataPort,
    InvalidDataPort,
    Connect(std::io::Error),
    Configure(std::io::Error),
    Read(std::io::Error),
    Write(std::io::Error),
    Crypto,
    Closed,
}
impl From<plist::Error> for MrpDataStreamError { fn from(v: plist::Error)->Self{Self::Plist(v)} }
impl From<EncryptedRtspError> for MrpDataStreamError { fn from(v: EncryptedRtspError)->Self{Self::Transport(v)} }

pub struct MrpDataStream {
    stream: TcpStream,
    out_key: [u8;32],
    in_key: [u8;32],
    out_counter: u64,
    in_counter: u64,
    send_seqno: u64,
    encrypted_carry: Vec<u8>,
    plain_carry: Vec<u8>,
    connected: bool,
}

impl MrpDataStream {
    pub fn setup(
        channel: &mut EncryptedRtspChannel,
        next_cseq: &SharedCseq,
        session_uri: &str,
        dacp_id: &str,
        active_remote: &str,
        receiver_ip: IpAddr,
        shared_secret: &[u8;32],
        controller: &MrpController,
    ) -> Result<Self, MrpDataStreamError> {
        let mut rng=rand::thread_rng();
        let seed=rng.next_u64() & 0x7fff_ffff_ffff_ffff;
        let channel_uuid=random_uuid(&mut rng);
        let client_uuid=random_uuid(&mut rng);

        let mut stream=Dictionary::new();
        stream.insert("type".into(), Value::Integer(MRP_STREAM_TYPE_REMOTE_CONTROL.into()));
        stream.insert("controlType".into(), Value::Integer(MRP_STREAM_CONTROL_TYPE.into()));
        stream.insert("channelID".into(), Value::String(channel_uuid));
        stream.insert("seed".into(), Value::Integer(seed.into()));
        stream.insert("clientUUID".into(), Value::String(client_uuid));
        stream.insert("clientTypeUUID".into(), Value::String(MRP_CLIENT_TYPE_UUID.into()));
        stream.insert("wantsDedicatedSocket".into(), Value::Boolean(true));
        let mut root=Dictionary::new();
        root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let mut body=Vec::new();
        Value::Dictionary(root).to_writer_binary(&mut body)?;

        let cseq=next_cseq.fetch_add(1,Ordering::SeqCst);
        let request=RtspRequest{
            method:"SETUP".into(), uri:session_uri.into(), cseq,
            user_agent:"AirPlay/670.6.2".into(), dacp_id:dacp_id.into(),
            active_remote:active_remote.into(), client_instance:None,
            content_type:Some("application/x-apple-binary-plist".into()), body,
        };
        let response=channel.exchange(&request.encode(),cseq)?;
        if response.status!=200 { return Err(MrpDataStreamError::Status(response.status)); }
        let value=Value::from_reader(Cursor::new(&response.body))?;
        let port=value.as_dictionary()
            .and_then(|d|d.get("streams")).and_then(Value::as_array)
            .and_then(|a|a.first()).and_then(Value::as_dictionary)
            .and_then(|d|d.get("dataPort")).and_then(Value::as_unsigned_integer)
            .or_else(|| value.as_dictionary().and_then(|d|d.get("dataPort")).and_then(Value::as_unsigned_integer))
            .ok_or(MrpDataStreamError::MissingDataPort)?;
        if !(1024..=65535).contains(&port){return Err(MrpDataStreamError::InvalidDataPort);}

        Self::connect(receiver_ip,port as u16,seed,shared_secret,controller)
    }

    fn connect(
        receiver_ip:IpAddr, port:u16, seed:u64, shared_secret:&[u8;32],
        controller:&MrpController
    )->Result<Self,MrpDataStreamError>{
        let addr=SocketAddr::new(receiver_ip,port);
        let stream=TcpStream::connect_timeout(&addr,Duration::from_secs(5))
            .map_err(MrpDataStreamError::Connect)?;
        stream.set_nodelay(true).map_err(MrpDataStreamError::Configure)?;
        stream.set_read_timeout(Some(Duration::from_millis(5))).map_err(MrpDataStreamError::Configure)?;
        stream.set_write_timeout(Some(Duration::from_secs(1))).map_err(MrpDataStreamError::Configure)?;

        let salt=format!("DataStream-Salt{seed}");
        let hk=Hkdf::<Sha512>::new(Some(salt.as_bytes()),shared_secret);
        let mut out_key=[0u8;32]; let mut in_key=[0u8;32];
        hk.expand(b"DataStream-Output-Encryption-Key",&mut out_key).map_err(|_|MrpDataStreamError::Crypto)?;
        hk.expand(b"DataStream-Input-Encryption-Key",&mut in_key).map_err(|_|MrpDataStreamError::Crypto)?;
        let mut s=Self{
            stream,out_key,in_key,out_counter:0,in_counter:0,
            send_seqno:0x1_0000_0000 | u64::from(rand::thread_rng().next_u32()),
            encrypted_carry:Vec::new(),plain_carry:Vec::new(),connected:true,
        };
        let state=controller.snapshot().map_err(|_|MrpDataStreamError::Crypto)?;
        let opening_generation = state.state_generation;
        for msg in state.build_type130_opening_messages(){
            s.send_protobuf(&msg)?;
        }
        // Pinned ap2_mrp_attach clears the dirty/artwork latches after the
        // complete opening handshake so the worker does not immediately
        // duplicate the initial SET_STATE push.
        controller
            .complete_type130_state_push(opening_generation, true)
            .map_err(|_| MrpDataStreamError::Crypto)?;
        Ok(s)
    }

    pub fn connected(&self)->bool{self.connected}

    pub fn tick(&mut self)->Result<(),MrpDataStreamError>{
        if !self.connected{return Ok(());}
        let mut tmp=[0u8;4096];
        loop{
            match self.stream.read(&mut tmp){
                Ok(0)=>{self.connected=false;return Err(MrpDataStreamError::Closed);}
                Ok(n)=>self.encrypted_carry.extend_from_slice(&tmp[..n]),
                Err(e) if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut)=>break,
                Err(e)=>{self.connected=false;return Err(MrpDataStreamError::Read(e));}
            }
        }
        self.decrypt_complete_frames()?;
        self.process_data_frames()
    }

    fn send_protobuf(&mut self,msg:&[u8])->Result<(),MrpDataStreamError>{
        let mut blob=Vec::new(); put_varint(&mut blob,msg.len() as u64); blob.extend_from_slice(msg);
        let mut params=Dictionary::new(); params.insert("data".into(),Value::Data(blob));
        let mut root=Dictionary::new(); root.insert("params".into(),Value::Dictionary(params));
        let mut plist=Vec::new(); Value::Dictionary(root).to_writer_binary(&mut plist)?;
        let frame=data_header(b"sync",b"comm",self.send_seqno,plist.len());
        let mut raw=Vec::with_capacity(DATA_HDR_LEN+plist.len());raw.extend_from_slice(&frame);raw.extend_from_slice(&plist);
        self.send_raw(&raw)
    }

    fn send_reply(&mut self,seq:u64)->Result<(),MrpDataStreamError>{
        self.send_raw(&data_header(b"rply",&[0,0,0,0],seq,0))
    }

    fn send_raw(&mut self,data:&[u8])->Result<(),MrpDataStreamError>{
        let wire=encrypt_frames(&self.out_key,&mut self.out_counter,data)?;
        self.stream.write_all(&wire).map_err(MrpDataStreamError::Write)
    }

    fn decrypt_complete_frames(&mut self)->Result<(),MrpDataStreamError>{
        let mut off=0usize;
        while self.encrypted_carry.len().saturating_sub(off)>=2{
            let len=u16::from_le_bytes([self.encrypted_carry[off],self.encrypted_carry[off+1]]) as usize;
            if len==0||len>FRAME_MAX{return Err(MrpDataStreamError::Crypto);}
            let total=2+len+TAG_SIZE;
            if self.encrypted_carry.len()-off<total{break;}
            let aad=&self.encrypted_carry[off..off+2];
            let ct=&self.encrypted_carry[off+2..off+total];
            let nonce=nonce(self.in_counter); self.in_counter=self.in_counter.wrapping_add(1);
            let plain=ChaCha20Poly1305::new(Key::from_slice(&self.in_key))
                .decrypt(Nonce::from_slice(&nonce),Payload{msg:ct,aad})
                .map_err(|_|MrpDataStreamError::Crypto)?;
            self.plain_carry.extend_from_slice(&plain);
            off+=total;
        }
        if off>0{self.encrypted_carry.drain(..off);}
        Ok(())
    }

    fn process_data_frames(&mut self)->Result<(),MrpDataStreamError>{
        let mut off=0usize;
        while self.plain_carry.len().saturating_sub(off)>=DATA_HDR_LEN{
            let h=&self.plain_carry[off..];
            let size=u32::from_be_bytes([h[0],h[1],h[2],h[3]]) as usize;
            if size<DATA_HDR_LEN||size>16*1024{return Err(MrpDataStreamError::Crypto);}
            if self.plain_carry.len()-off<size{break;}
            let seq=u64::from_be_bytes(h[20..28].try_into().unwrap());
            if &h[4..8]==b"sync"{ self.send_reply(seq)?; }
            off+=size;
        }
        if off>0{self.plain_carry.drain(..off);}
        Ok(())
    }
}


pub struct MrpDataStreamWorker {
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl MrpDataStreamWorker {
    pub fn start(mut stream: MrpDataStream, controller: MrpController) -> Self {
        let stop=Arc::new(AtomicBool::new(false));
        let healthy=Arc::new(AtomicBool::new(true));
        let stop_t=Arc::clone(&stop); let healthy_t=Arc::clone(&healthy);
        let worker=thread::spawn(move||{
            let mut last_push=Instant::now();
            while !stop_t.load(Ordering::SeqCst) {
                if stream.tick().is_err() {
                    healthy_t.store(false,Ordering::SeqCst);
                    break;
                }

                // Pinned ap2_mrp_prepare_state_push: send immediately when
                // mutable MRP state is dirty; otherwise re-push every 15s
                // only while playback is actually PLAYING.
                let periodic_due = last_push.elapsed() >= Duration::from_secs(15);
                match controller.try_publication_guard() {
                    Ok(Some(_publish)) => match controller.prepare_type130_state_push(periodic_due) {
                        Ok(Some((msg, generation))) => {
                            let sent = stream.send_protobuf(&msg).is_ok();
                            let _ = controller.complete_type130_state_push(generation, sent);
                            if !sent {
                                healthy_t.store(false,Ordering::SeqCst);
                                break;
                            }
                            last_push = Instant::now();
                        }
                        Ok(None) => {}
                        Err(_) => {
                            healthy_t.store(false,Ordering::SeqCst);
                            break;
                        }
                    },
                    Ok(None) => {
                        // Exact pinned behavior: state push defers behind
                        // metadata/artwork publication instead of blocking.
                    }
                    Err(_) => {
                        healthy_t.store(false,Ordering::SeqCst);
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(25));
            }
            if stream.connected() {
                if let Ok(state)=controller.snapshot() {
                    let _=stream.send_protobuf(&state.build_type130_disconnect_message());
                }
            }
        });
        Self{stop,healthy,worker:Some(worker)}
    }
    pub fn healthy(&self)->bool{self.healthy.load(Ordering::SeqCst)}
    pub fn stop(&mut self){
        self.stop.store(true,Ordering::SeqCst);
        if let Some(w)=self.worker.take(){let _=w.join();}
    }
}
impl Drop for MrpDataStreamWorker { fn drop(&mut self){self.stop();} }

fn encrypt_frames(key:&[u8;32],counter:&mut u64,input:&[u8])->Result<Vec<u8>,MrpDataStreamError>{
    let cipher=ChaCha20Poly1305::new(Key::from_slice(key));
    let mut out=Vec::with_capacity(input.len()+((input.len()+FRAME_MAX-1)/FRAME_MAX)*(2+TAG_SIZE));
    for chunk in input.chunks(FRAME_MAX){
        let len=(chunk.len() as u16).to_le_bytes();
        let n=nonce(*counter);*counter=counter.wrapping_add(1);
        let ct=cipher.encrypt(Nonce::from_slice(&n),Payload{msg:chunk,aad:&len})
            .map_err(|_|MrpDataStreamError::Crypto)?;
        out.extend_from_slice(&len);out.extend_from_slice(&ct);
    }
    Ok(out)
}

fn nonce(counter:u64)->[u8;12]{let mut n=[0u8;12];n[4..].copy_from_slice(&counter.to_le_bytes());n}
fn data_header(kind:&[u8;4],cmd:&[u8;4],seq:u64,payload_len:usize)->[u8;32]{
    let mut h=[0u8;32];let size=(DATA_HDR_LEN+payload_len) as u32;
    h[..4].copy_from_slice(&size.to_be_bytes());h[4..8].copy_from_slice(kind);
    h[16..20].copy_from_slice(cmd);h[20..28].copy_from_slice(&seq.to_be_bytes());h
}
fn put_varint(out:&mut Vec<u8>,mut v:u64){loop{let mut b=(v&0x7f) as u8;v>>=7;if v!=0{b|=0x80;}out.push(b);if v==0{break;}}}
fn random_uuid(rng:&mut impl RngCore)->String{
    let mut b=[0u8;16];rng.fill_bytes(&mut b);
    format!("{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15])
}

#[cfg(test)]
mod tests{
 use super::*;
 #[test] fn datastream_header_matches_pinned_big_endian_shape(){
  let h=data_header(b"sync",b"comm",0x0102030405060708,5);
  assert_eq!(u32::from_be_bytes(h[..4].try_into().unwrap()),37);
  assert_eq!(&h[4..8],b"sync");assert_eq!(&h[16..20],b"comm");
  assert_eq!(&h[20..28],&0x0102030405060708u64.to_be_bytes());
 }
 #[test] fn hkdf_and_hap_frames_round_trip_shape(){
  let key=[7u8;32];let mut c=0;let wire=encrypt_frames(&key,&mut c,b"abc").unwrap();
  assert_eq!(u16::from_le_bytes([wire[0],wire[1]]),3);assert_eq!(c,1);
 }
}
