use std::io::{Read, Write};
use super::contracts::{self, Action, CorrelationId, MessageEnvelope, MessageKind};

pub const header_len: usize = 28;
const MAGIC: &[u8; 4] = b"FXTH";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError { TruncatedFrame, InvalidFrameMagic, InvalidFrameKind, InvalidFrameSubject, InvalidHostFrame, HostFrameTooLarge, InvalidPayload, Contract(contracts::Error), Io }
impl From<contracts::Error> for DecodeError { fn from(e: contracts::Error)->Self{Self::Contract(e)} }

#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct Header{pub envelope:MessageEnvelope}
impl Header{
 pub fn decode(bytes:&[u8])->Result<Self,DecodeError>{if bytes.len()<header_len{return Err(DecodeError::TruncatedFrame)}if &bytes[..4]!=MAGIC{return Err(DecodeError::InvalidFrameMagic)}let revision=u16::from_le_bytes([bytes[4],bytes[5]]);let kind=decode_kind(bytes[6],bytes[7])?;let required_capabilities=u64::from_le_bytes(bytes[8..16].try_into().unwrap());let correlation=u64::from_le_bytes(bytes[16..24].try_into().unwrap());let payload_len=u32::from_le_bytes(bytes[24..28].try_into().unwrap());let envelope=MessageEnvelope{revision,kind,required_capabilities,correlation_id:(correlation!=0).then_some(CorrelationId{value:correlation}),payload_len};envelope.validate(payload_len as usize)?;Ok(Self{envelope})}
 pub fn encode(self)->Result<[u8;header_len],DecodeError>{self.envelope.validate(self.envelope.payload_len as usize)?;let(k,s)=encode_kind(self.envelope.kind);let mut b=[0;header_len];b[..4].copy_from_slice(MAGIC);b[4..6].copy_from_slice(&self.envelope.revision.to_le_bytes());b[6]=k;b[7]=s;b[8..16].copy_from_slice(&self.envelope.required_capabilities.to_le_bytes());b[16..24].copy_from_slice(&self.envelope.correlation_id.map_or(0,|x|x.value).to_le_bytes());b[24..28].copy_from_slice(&self.envelope.payload_len.to_le_bytes());Ok(b)}
}
#[derive(Clone,Debug,Eq,PartialEq)] pub struct EncodedFrame{pub bytes:Vec<u8>}
#[derive(Clone,Debug,Eq,PartialEq)] pub struct DecodedFrame{pub envelope:MessageEnvelope,pub payload:Vec<u8>}
impl DecodedFrame{pub fn message(&self)->(&MessageEnvelope,&[u8]){(&self.envelope,&self.payload)}}
pub fn encode_frame(revision:u16,required_capabilities:u64,correlation_id:Option<CorrelationId>,kind:MessageKind,payload:&[u8])->Result<EncodedFrame,DecodeError>{if payload.len()>contracts::max_host_frame_bytes{return Err(DecodeError::HostFrameTooLarge)}let envelope=MessageEnvelope{revision,kind,required_capabilities,correlation_id,payload_len:payload.len() as u32};let header=Header{envelope}.encode()?;let mut bytes=Vec::with_capacity(header_len+payload.len());bytes.extend_from_slice(&header);bytes.extend_from_slice(payload);Ok(EncodedFrame{bytes})}
pub fn decode_frame(bytes:&[u8])->Result<DecodedFrame,DecodeError>{let header=Header::decode(bytes)?;let expected=header_len.checked_add(header.envelope.payload_len as usize).ok_or(DecodeError::HostFrameTooLarge)?;if bytes.len()<expected{return Err(DecodeError::TruncatedFrame)}if bytes.len()!=expected{return Err(DecodeError::InvalidHostFrame)}Ok(DecodedFrame{envelope:header.envelope,payload:bytes[header_len..].to_vec()})}
pub fn read_frame(reader:&mut impl Read)->Result<DecodedFrame,DecodeError>{let mut h=[0;header_len];reader.read_exact(&mut h).map_err(|_|DecodeError::TruncatedFrame)?;let header=Header::decode(&h)?;let mut bytes=h.to_vec();bytes.resize(header_len+header.envelope.payload_len as usize,0);reader.read_exact(&mut bytes[header_len..]).map_err(|_|DecodeError::TruncatedFrame)?;decode_frame(&bytes)}
pub fn write_frame(writer:&mut impl Write,frame:&EncodedFrame)->Result<(),DecodeError>{writer.write_all(&frame.bytes).and_then(|_|writer.flush()).map_err(|_|DecodeError::Io)}
fn encode_kind(k:MessageKind)->(u8,u8){match k{MessageKind::Hello=>(0,0),MessageKind::Request(a)=>(1,a as u8),MessageKind::Response(a)=>(2,a as u8),MessageKind::Cancel=>(4,0)}}
fn action(s:u8)->Result<Action,DecodeError>{Ok(match s{0=>Action::Start,1=>Action::Read,2=>Action::Screen,3=>Action::Write,4=>Action::Wait,5=>Action::Monitor,6=>Action::Inspect,7=>Action::List,8=>Action::Resize,9=>Action::Signal,10=>Action::Close,_=>return Err(DecodeError::InvalidFrameSubject)})}
fn decode_kind(k:u8,s:u8)->Result<MessageKind,DecodeError>{match k{0 if s==0=>Ok(MessageKind::Hello),0=>Err(DecodeError::InvalidFrameSubject),1=>Ok(MessageKind::Request(action(s)?)),2=>Ok(MessageKind::Response(action(s)?)),3=>Err(DecodeError::InvalidFrameKind),4 if s==0=>Ok(MessageKind::Cancel),4=>Err(DecodeError::InvalidFrameSubject),_=>Err(DecodeError::InvalidFrameKind)}}

#[cfg(test)]mod tests{use super::*;#[test]fn header_round_trip(){let e=MessageEnvelope{revision:5,kind:MessageKind::Request(Action::Screen),required_capabilities:1,correlation_id:Some(CorrelationId{value:41}),payload_len:2};assert_eq!(Header::decode(&Header{envelope:e}.encode().unwrap()).unwrap().envelope,e)}#[test]fn frame_round_trip(){let f=encode_frame(5,0,Some(CorrelationId{value:1}),MessageKind::Cancel,b"{}").unwrap();assert_eq!(decode_frame(&f.bytes).unwrap().payload,b"{}")}#[test]fn failures(){assert_eq!(Header::decode(b""),Err(DecodeError::TruncatedFrame));let mut b=[0;28];assert_eq!(Header::decode(&b),Err(DecodeError::InvalidFrameMagic));b[..4].copy_from_slice(MAGIC);b[4..6].copy_from_slice(&5u16.to_le_bytes());b[6]=3;assert_eq!(Header::decode(&b),Err(DecodeError::InvalidFrameKind))}}
