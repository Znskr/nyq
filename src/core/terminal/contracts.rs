use std::{cmp::Ordering, path::Path};
use crate::core::session::session_layout;

pub const max_authority_text_bytes: usize = 4096;
pub const max_command_bytes: usize = 64 * 1024;
pub const max_shell_path_bytes: usize = 4096;
pub const max_write_bytes: usize = 64 * 1024;
pub const max_write_items: usize = 4096;
pub const max_match_bytes: usize = 4096;
pub const max_monitor_id_bytes: usize = 128;
pub const max_monitor_definitions: usize = 32;
pub const max_list_results: usize = 256;
pub const max_cell_text_bytes: usize = 64;
pub const max_render_snapshot_bytes: usize = 8 * 1024 * 1024;
pub const max_checkpoint_payload_bytes: usize = 64 * 1024 * 1024;
pub const max_host_frame_bytes: usize = 1024 * 1024;
pub const max_dimension: u16 = 4096;
pub const current_protocol_revision: u16 = 5;
pub const previous_protocol_revision: u16 = 4;
pub const oldest_hello_revision: u16 = 1;
pub const compatibility_hello_revision: u16 = previous_protocol_revision - 1;
pub const protocol_capability_authority_generations: u64 = 1 << 0;
pub const protocol_capability_screen_checkpoints: u64 = 1 << 1;
pub const protocol_capability_tmux_recovery: u64 = 1 << 2;
pub const protocol_capability_path_outside_workspace_error: u64 = 1 << 3;
pub const protocol_capability_complete_process_tree_signals: u64 = 1 << 4;
pub const known_protocol_capabilities: u64 = (1 << 5) - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error { InvalidLifecycleTransition, InvalidAttentionState, InvalidReturnCondition, InvalidSchedule, InvalidShell, InvalidControlInput, InvalidWritePayload, InvalidMonitorCondition, InvalidMonitorLifetime, MissingCheckSchedule, UnexpectedCheckSchedule, InvalidMonitor, InvalidCommand, InvalidCwd, MissingReturnCondition, MissingWaitCeiling, InvalidWaitCeiling, InvalidDimensions, InvalidSessionId, InvalidRawCursor, InvalidRawRange, InvalidRawGap, InvalidRenderSnapshot, InvalidRenderCell, RenderSnapshotTooLarge, InvalidCheckpoint, CheckpointTooLarge, CheckpointChecksumMismatch, InvalidPrincipal, InvalidAuthorityGeneration, AuthorityGenerationExhausted, InvalidAuthorityGrant, InvalidAuthorityClaim, InvalidHolderProof, InvalidStartPersistence, InvalidProcessOwner, InvalidListFilter, InvalidProtocolRange, UnknownRequiredCapability, InvalidRequiredCapabilities, InvalidCorrelationId, UnsupportedProtocolRevision, HostFrameTooLarge, InvalidHostFrame, MissingCorrelationId, UnexpectedCorrelationId, MessageKindMismatch, InvalidResult, InvalidReturnOutcome }
pub type Result<T> = std::result::Result<T, Error>;

#[repr(u8)] #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action { Start, Read, Screen, Write, Wait, Monitor, Inspect, List, Resize, Signal, Close }
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Lifecycle { Starting, Running, Exited, Lost, Closed }
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum LifecycleEvent { ChildStarted, ChildExited, HostLost, Close }
pub fn transition_lifecycle(c: Lifecycle, e: LifecycleEvent) -> Result<Lifecycle> { use Lifecycle::*; use LifecycleEvent::*; Ok(match (c,e) { (Starting,ChildStarted)=>Running,(Starting,ChildExited)|(Running,ChildExited)=>Exited,(Starting,HostLost)|(Running,HostLost)=>Lost,(Starting,Close)|(Running,Close)|(Exited,Close)|(Lost,Close)=>Closed,_=>return Err(Error::InvalidLifecycleTransition) }) }
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub enum Attention { #[default] Background, AgentWait, UserTakeover }
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum ActorRole { Human, Agent }
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub enum WriteLease { #[default] None, Human, Agent }
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub struct AttentionState { pub attention: Attention, pub write_lease: WriteLease }
impl AttentionState { pub fn validate(self)->Result<()> { if (self.attention==Attention::UserTakeover)!=(self.write_lease==WriteLease::Human){Err(Error::InvalidAttentionState)}else{Ok(())} } }
pub fn cancel_attention(mut s:AttentionState,a:ActorRole)->AttentionState { match a { ActorRole::Human=>{if s.attention==Attention::UserTakeover{s.attention=Attention::Background}if s.write_lease==WriteLease::Human{s.write_lease=WriteLease::None}},ActorRole::Agent=>{if s.attention==Attention::AgentWait{s.attention=Attention::Background}if s.write_lease==WriteLease::Agent{s.write_lease=WriteLease::None}}};s }
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Backend { Native, Tmux }
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum PersistenceLevel { Durable }
#[derive(Clone, Debug, Eq, PartialEq)] pub enum ReturnCondition { Started, Exit, Quiet(u64), Match(Vec<u8>) }
impl ReturnCondition { pub fn validate(&self)->Result<()>{match self{Self::Quiet(0)=>Err(Error::InvalidReturnCondition),Self::Match(v) if v.is_empty()||v.len()>max_match_bytes=>Err(Error::InvalidReturnCondition),_=>Ok(())}} }
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct PollSchedule{pub interval_ms:u64} impl PollSchedule{pub fn validate(self)->Result<()>{if self.interval_ms==0{Err(Error::InvalidSchedule)}else{Ok(())}}}
#[derive(Clone, Debug, Eq, PartialEq)] pub enum ShellSpec{UserLogin,Executable{path:Vec<u8>,clean_start:bool}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum NamedKey{Enter,Tab,Escape,Backspace,Delete,Insert,ArrowUp,ArrowDown,ArrowLeft,ArrowRight,Home,End,PageUp,PageDown}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct ControlInput{pub character:u8} impl ControlInput{pub fn validate(self)->Result<()>{if (b'@'..=b'_').contains(&self.character)||(b'a'..=b'z').contains(&self.character)||self.character==b'?'{Ok(())}else{Err(Error::InvalidControlInput)}}}
#[derive(Clone, Debug, Eq, PartialEq)] pub enum WritePayload{Text(Vec<u8>),Keys(Vec<NamedKey>),Controls(Vec<ControlInput>),Paste(Vec<u8>)}
impl WritePayload{pub fn validate(&self)->Result<()>{let n=match self{Self::Text(v)|Self::Paste(v)=>{if v.is_empty()||v.len()>max_write_bytes{return Err(Error::InvalidWritePayload)}v.len()},Self::Keys(v)=>v.len(),Self::Controls(v)=>{if v.iter().any(|x|x.validate().is_err()){return Err(Error::InvalidWritePayload)}v.len()}};if n==0||n>max_write_items{Err(Error::InvalidWritePayload)}else{Ok(())}}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Signal{Hangup,Interrupt,Quit,Terminate,Kill}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum ClosePolicy{Graceful,Force}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum NotifySchedule{OnMatch,OnStateChange,OnExit,EveryCheck,EveryNChecks(u32),Interval(PollSchedule)}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum MonitorLifetime{UntilMatch,UntilSessionEnd,DurationMs(u64)}
#[derive(Clone, Debug, Eq, PartialEq)] pub enum MonitorCondition{ProcessExit,ExitCode(i32),Signal(Signal),OutputContains(Vec<u8>),OutputMatches(Vec<u8>),OutputQuietMs(u64),ScreenMatches(Vec<u8>),TcpReady{host:Vec<u8>,port:u16},HttpReady(Vec<u8>),PathExists(Vec<u8>),PathChanged(Vec<u8>),PathSize{path:Vec<u8>,minimum_bytes:u64},CustomProbe{command:Vec<u8>,cwd:Vec<u8>}}
impl MonitorCondition{pub fn requires_polling(&self)->bool{matches!(self,Self::TcpReady{..}|Self::HttpReady(_)|Self::PathExists(_)|Self::PathChanged(_)|Self::PathSize{..}|Self::CustomProbe{..})}}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct MonitorDefinition{pub condition:MonitorCondition,pub check_schedule:Option<PollSchedule>,pub notify_schedule:NotifySchedule,pub lifetime:MonitorLifetime}
#[derive(Clone, Debug, Eq, PartialEq)] pub enum MonitorOperation{Add(MonitorDefinition),Update{monitor_id:Vec<u8>,definition:MonitorDefinition},Pause(Vec<u8>),Resume(Vec<u8>),Remove(Vec<u8>)}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct Dimensions{pub rows:u16,pub columns:u16} impl Dimensions{pub fn validate(self)->Result<()>{if self.rows==0||self.columns==0||self.rows>max_dimension||self.columns>max_dimension{Err(Error::InvalidDimensions)}else{Ok(())}}}

#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct RawCursor{pub segment:u64,pub offset:u64} impl RawCursor{pub fn validate(self)->Result<()>{if self.segment==0{Err(Error::InvalidRawCursor)}else{Ok(())}}}
pub fn compare_raw_cursors(a:RawCursor,b:RawCursor)->Ordering{(a.segment,a.offset).cmp(&(b.segment,b.offset))}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct RawRange{pub start:RawCursor,pub end:RawCursor} impl RawRange{pub fn validate(self)->Result<()>{if self.start.validate().is_err()||self.end.validate().is_err()||self.start.segment!=self.end.segment||self.start>self.end{Err(Error::InvalidRawRange)}else{Ok(())}}}
impl PartialOrd for RawCursor{fn partial_cmp(&self,o:&Self)->Option<Ordering>{Some(self.cmp(o))}} impl Ord for RawCursor{fn cmp(&self,o:&Self)->Ordering{compare_raw_cursors(*self,*o)}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct RawGap{pub missing_from:RawCursor,pub available_from:RawCursor}
impl RawGap{pub fn validate(self)->Result<()>{if self.missing_from.validate().is_err()||self.available_from.validate().is_err()||self.missing_from>=self.available_from{Err(Error::InvalidRawGap)}else{Ok(())}}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum CursorShape{Block,Underline,Bar}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct RenderCursor{pub row:u16,pub column:u16,pub visible:bool,pub shape:CursorShape,pub blinking:bool}
impl Default for RenderCursor{fn default()->Self{Self{row:0,column:0,visible:true,shape:CursorShape::Block,blinking:true}}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct TerminalModes{pub alternate_screen:bool,pub origin:bool,pub autowrap:bool,pub insert:bool,pub bracketed_paste:bool,pub mouse_tracking:bool,pub focus_tracking:bool,pub application_cursor_keys:bool,pub application_keypad:bool,pub keyboard_protocol:bool,pub synchronized_updates:bool}
impl Default for TerminalModes{fn default()->Self{Self{alternate_screen:false,origin:false,autowrap:true,insert:false,bracketed_paste:false,mouse_tracking:false,focus_tracking:false,application_cursor_keys:false,application_keypad:false,keyboard_protocol:false,synchronized_updates:false}}}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub enum CellColor{#[default] Default,Indexed(u8),Rgb{red:u8,green:u8,blue:u8}}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub struct CellStyle{pub foreground:CellColor,pub background:CellColor,pub bold:bool,pub faint:bool,pub italic:bool,pub underline:bool,pub inverse:bool,pub strikethrough:bool}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub enum CellKind{#[default] Blank,Single,Wide,Continuation}
#[derive(Clone, Debug, Default, Eq, PartialEq)] pub struct RenderCell{pub kind:CellKind,pub text:Vec<u8>,pub style:CellStyle}
pub const max_render_cells:usize=max_render_snapshot_bytes/std::mem::size_of::<RenderCell>();
#[derive(Clone, Debug, Eq, PartialEq)] pub struct RenderSnapshot{pub dimensions:Dimensions,pub cursor:RenderCursor,pub modes:TerminalModes,pub cells:Vec<RenderCell>}
impl RenderSnapshot{pub fn validate(&self)->Result<()>{self.dimensions.validate()?;if self.cursor.row>=self.dimensions.rows||self.cursor.column>=self.dimensions.columns||self.cells.len()!=self.dimensions.rows as usize*self.dimensions.columns as usize{return Err(Error::InvalidRenderSnapshot)};for(i,c)in self.cells.iter().enumerate(){if c.text.len()>max_cell_text_bytes{return Err(Error::RenderSnapshotTooLarge)};match c.kind{CellKind::Blank|CellKind::Continuation if !c.text.is_empty()=>return Err(Error::InvalidRenderCell),CellKind::Single|CellKind::Wide if c.text.is_empty()||std::str::from_utf8(&c.text).is_err()=>return Err(Error::InvalidRenderCell),CellKind::Wide if i%self.dimensions.columns as usize+1>=self.dimensions.columns as usize||self.cells[i+1].kind!=CellKind::Continuation=>return Err(Error::InvalidRenderCell),CellKind::Continuation if i%self.dimensions.columns as usize==0||self.cells[i-1].kind!=CellKind::Wide=>return Err(Error::InvalidRenderCell),_=>{}}}Ok(())}}
pub type OwnedRenderSnapshot=RenderSnapshot;
pub type CheckpointChecksum=[u8;32];
pub fn checkpoint_checksum(payload:&[u8])->CheckpointChecksum{crate::core::workspace::read_tracker::content_hash(payload)}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct CheckpointEnvelope{pub engine_schema_revision:u16,pub applied_cursor:RawCursor,pub payload_len:u32,pub checksum:CheckpointChecksum}
impl CheckpointEnvelope{pub fn validate_header(&self)->Result<()>{if self.engine_schema_revision==0||self.payload_len==0||self.applied_cursor.validate().is_err(){Err(Error::InvalidCheckpoint)}else if self.payload_len as usize>max_checkpoint_payload_bytes{Err(Error::CheckpointTooLarge)}else{Ok(())}}pub fn validate(&self,payload:&[u8])->Result<()>{self.validate_header()?;if self.payload_len as usize!=payload.len(){Err(Error::InvalidCheckpoint)}else if self.checksum!=checkpoint_checksum(payload){Err(Error::CheckpointChecksumMismatch)}else{Ok(())}}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum ScreenUnavailableReason{Missing,Corrupt,UnsupportedSchema,RetentionEvicted,RawGap,ResizeUncheckpointed}
#[derive(Clone, Debug, Eq, PartialEq)] pub enum ScreenRecovery{Available(CheckpointEnvelope),Unavailable(ScreenUnavailableReason)}

#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum TransportRole{Interactive,Headless,Acp}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum TerminalLifetime{Session}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct Principal{pub profile_user:Vec<u8>,pub durable_session_id:Vec<u8>,pub workspace_root:Vec<u8>,pub cwd:Vec<u8>,pub transport_role:TransportRole,pub backend:Backend,pub lifetime:TerminalLifetime}
impl Principal{pub fn validate(&self)->Result<()>{for v in [&self.profile_user,&self.workspace_root,&self.cwd]{if !valid_bounded_text(v,max_authority_text_bytes){return Err(Error::InvalidPrincipal)}}validate_session_id(&self.durable_session_id)}pub fn eql(&self,o:&Self)->bool{self==o}}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct OwnerCatalogPrincipal{pub profile_user:Vec<u8>,pub durable_session_id:Vec<u8>,pub workspace_root:Vec<u8>,pub transport_role:TransportRole}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct OwnerCatalogProof{pub bytes:[u8;32]}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct OwnerCatalogAuthorityClaim{pub principal:OwnerCatalogPrincipal,pub actor:ActorRole,pub proof:OwnerCatalogProof}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)] pub struct AllowedControls{pub read:bool,pub screen:bool,pub write:bool,pub wait:bool,pub monitor:bool,pub inspect:bool,pub list:bool,pub resize:bool,pub signal:bool,pub close:bool}
impl AllowedControls{pub fn observer()->Self{Self{read:true,screen:true,inspect:true,list:true,..Self::default()}}pub fn full()->Self{Self{read:true,screen:true,write:true,wait:true,monitor:true,inspect:true,list:true,resize:true,signal:true,close:true}}pub fn human_takeover()->Self{Self{screen:true,write:true,resize:true,..Self::default()}}pub fn any(self)->bool{self!=Self::default()}pub fn allows(self,a:Action)->bool{match a{Action::Start=>false,Action::Read=>self.read,Action::Screen=>self.screen,Action::Write=>self.write,Action::Wait=>self.wait,Action::Monitor=>self.monitor,Action::Inspect=>self.inspect,Action::List=>self.list,Action::Resize=>self.resize,Action::Signal=>self.signal,Action::Close=>self.close}}}
pub fn intersect_controls(a:AllowedControls,b:AllowedControls)->AllowedControls{AllowedControls{read:a.read&&b.read,screen:a.screen&&b.screen,write:a.write&&b.write,wait:a.wait&&b.wait,monitor:a.monitor&&b.monitor,inspect:a.inspect&&b.inspect,list:a.list&&b.list,resize:a.resize&&b.resize,signal:a.signal&&b.signal,close:a.close&&b.close}}
pub fn lifecycle_controls(l:Lifecycle)->AllowedControls{match l{Lifecycle::Starting|Lifecycle::Running=>AllowedControls::full(),Lifecycle::Exited|Lifecycle::Lost=>AllowedControls{read:true,screen:true,wait:true,inspect:true,list:true,close:true,..Default::default()},Lifecycle::Closed=>AllowedControls::observer()}}
pub fn project_next_actions(l:Lifecycle,g:AllowedControls,a:ActorRole,s:AttentionState)->AllowedControls{let mut p=intersect_controls(lifecycle_controls(l),g);let lease=if a==ActorRole::Human{WriteLease::Human}else{WriteLease::Agent};if s.write_lease!=WriteLease::None&&s.write_lease!=lease{p.write=false}p}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct HolderProof{pub bytes:[u8;32]}
impl HolderProof{pub fn validate(self)->Result<()>{if self.bytes.iter().any(|&b|b!=0){Ok(())}else{Err(Error::InvalidHolderProof)}}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct AuthorityGeneration{pub value:u64} impl AuthorityGeneration{pub fn init(v:u64)->Result<Self>{if v==0{Err(Error::InvalidAuthorityGeneration)}else{Ok(Self{value:v})}}pub fn next(self)->Result<Self>{Self::init(self.value.checked_add(1).ok_or(Error::AuthorityGenerationExhausted)?)}}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct RepeatedProbeAuthority{pub command:Vec<u8>,pub cwd:Vec<u8>,pub check_schedule:PollSchedule,pub notify_schedule:NotifySchedule,pub lifetime:MonitorLifetime}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct AuthorityGrant{pub principal:Principal,pub actor:ActorRole,pub controls:AllowedControls,pub generation:AuthorityGeneration,pub repeated_probes:Vec<RepeatedProbeAuthority>}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct ProcessOwner{pub pid:i32,pub process_token:[u8;128],pub process_token_len:u8}
impl ProcessOwner{pub fn init(pid:i32,token:&[u8])->Result<Self>{if pid<=0||token.is_empty()||token.len()>128{return Err(Error::InvalidProcessOwner)}let mut process_token=[0;128];process_token[..token.len()].copy_from_slice(token);Ok(Self{pid,process_token,process_token_len:token.len() as u8})}pub fn token(&self)->&[u8]{&self.process_token[..self.process_token_len as usize]}pub fn validate(&self)->Result<()>{if self.pid<=0||self.process_token_len==0{Err(Error::InvalidProcessOwner)}else{Ok(())}}}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct AuthorityClaim{pub principal:Principal,pub actor:ActorRole,pub generation:AuthorityGeneration,pub proof:HolderProof,pub process_owner:Option<ProcessOwner>}
impl AuthorityClaim{pub fn validate(&self)->Result<()>{self.principal.validate()?;if self.generation.value==0||self.proof.validate().is_err()||self.process_owner.as_ref().is_some_and(|p|p.validate().is_err()){Err(Error::InvalidAuthorityClaim)}else{Ok(())}}}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct StartPersistence{pub grant:AuthorityGrant,pub proof:HolderProof,pub direct_human_model_read_only:bool}

#[derive(Clone, Debug, Eq, PartialEq)] pub struct StartRequest{pub cwd:Vec<u8>,pub command:Option<Vec<u8>>,pub shell:ShellSpec,pub backend:Backend,pub return_when:Option<ReturnCondition>,pub wait_ceiling_ms:Option<u64>,pub dimensions:Option<Dimensions>,pub initial_monitors:Vec<MonitorDefinition>,pub persistence:Option<StartPersistence>}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct ReadRequest{pub session_id:Vec<u8>,pub cursor:RawCursor,pub authority:Option<AuthorityClaim>}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct SessionRequest{pub session_id:Vec<u8>,pub authority:Option<AuthorityClaim>,pub after_event_id:u64,pub acknowledge_event_id:Option<u64>,pub max_events:u16}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum WriteLeaseIntent{Acquire,Use,Release,Revoke}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct WriteRequest{pub session_id:Vec<u8>,pub payload:Option<WritePayload>,pub lease:WriteLeaseIntent,pub authority:Option<AuthorityClaim>}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct WaitRequest{pub session_id:Vec<u8>,pub return_when:ReturnCondition,pub safety_ceiling_ms:u64,pub authority:Option<AuthorityClaim>}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct MonitorRequest{pub session_id:Vec<u8>,pub operation:MonitorOperation,pub authority:Option<AuthorityClaim>}
#[derive(Clone, Debug, Default, Eq, PartialEq)] pub struct ListFilters{pub task_id:Option<Vec<u8>>,pub workspace_root:Option<Vec<u8>>,pub lifecycle:Option<Lifecycle>,pub backend:Option<Backend>,pub owner_authority:Option<OwnerCatalogAuthorityClaim>}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct ResizeRequest{pub session_id:Vec<u8>,pub dimensions:Dimensions,pub authority:Option<AuthorityClaim>}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct SignalRequest{pub session_id:Vec<u8>,pub signal:Signal,pub authority:Option<AuthorityClaim>}
#[derive(Clone, Debug, Eq, PartialEq)] pub struct CloseRequest{pub session_id:Vec<u8>,pub policy:ClosePolicy,pub authority:Option<AuthorityClaim>}
#[derive(Clone, Debug, Eq, PartialEq)] pub enum ActionRequest{Start(StartRequest),Read(ReadRequest),Screen(SessionRequest),Write(WriteRequest),Wait(WaitRequest),Monitor(MonitorRequest),Inspect(SessionRequest),List(ListFilters),Resize(ResizeRequest),Signal(SignalRequest),Close(CloseRequest)}
impl ActionRequest{pub fn action(&self)->Action{match self{Self::Start(_)=>Action::Start,Self::Read(_)=>Action::Read,Self::Screen(_)=>Action::Screen,Self::Write(_)=>Action::Write,Self::Wait(_)=>Action::Wait,Self::Monitor(_)=>Action::Monitor,Self::Inspect(_)=>Action::Inspect,Self::List(_)=>Action::List,Self::Resize(_)=>Action::Resize,Self::Signal(_)=>Action::Signal,Self::Close(_)=>Action::Close}}}
impl ActionRequest{pub fn validate(&self)->Result<()>{match self{Self::Start(s)=>{if !valid_bounded_text(&s.cwd,max_authority_text_bytes)||!is_absolute(&s.cwd){return Err(Error::InvalidCwd)}if s.command.as_ref().is_some_and(|c|!valid_bounded_text(c,max_command_bytes)){return Err(Error::InvalidCommand)}if s.command.is_some()&&s.return_when.is_none(){return Err(Error::MissingReturnCondition)}if s.wait_ceiling_ms==Some(0){return Err(Error::InvalidWaitCeiling)}},Self::Read(r)=>{validate_session_id(&r.session_id)?;r.cursor.validate()?},Self::Screen(r)=>validate_session_id(&r.session_id)?,Self::Write(r)=>{validate_session_id(&r.session_id)?;if (r.lease==WriteLeaseIntent::Use)!=r.payload.is_some(){return Err(Error::InvalidWritePayload)}if let Some(p)=&r.payload{p.validate()?}},Self::Wait(r)=>{validate_session_id(&r.session_id)?;r.return_when.validate()?;if r.safety_ceiling_ms==0{return Err(Error::InvalidWaitCeiling)}},Self::Monitor(r)=>validate_session_id(&r.session_id)?,Self::Inspect(r)=>validate_session_id(&r.session_id)?,Self::List(_)=>{},Self::Resize(r)=>{validate_session_id(&r.session_id)?;r.dimensions.validate()?},Self::Signal(r)=>validate_session_id(&r.session_id)?,Self::Close(r)=>validate_session_id(&r.session_id)?}if let Some(c)=authority_claim(self){c.validate()?}Ok(())}}
pub fn authority_claim(r:&ActionRequest)->Option<&AuthorityClaim>{match r{ActionRequest::Read(x)=>x.authority.as_ref(),ActionRequest::Screen(x)=>x.authority.as_ref(),ActionRequest::Write(x)=>x.authority.as_ref(),ActionRequest::Wait(x)=>x.authority.as_ref(),ActionRequest::Monitor(x)=>x.authority.as_ref(),ActionRequest::Inspect(x)=>x.authority.as_ref(),ActionRequest::Resize(x)=>x.authority.as_ref(),ActionRequest::Signal(x)=>x.authority.as_ref(),ActionRequest::Close(x)=>x.authority.as_ref(),_=>None}}
pub type OwnedActionRequest=ActionRequest;
pub fn required_capabilities(r:&ActionRequest)->u64{protocol_capability_authority_generations|match r{ActionRequest::Start(s)=>protocol_capability_complete_process_tree_signals|if s.backend==Backend::Tmux{protocol_capability_tmux_recovery}else{0},ActionRequest::Signal(_)=>protocol_capability_complete_process_tree_signals,ActionRequest::Close(c)if c.policy==ClosePolicy::Force=>protocol_capability_complete_process_tree_signals,_=>0}}

#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct ProtocolRange{pub minimum:u16,pub current:u16} pub const local_protocol_range:ProtocolRange=ProtocolRange{minimum:previous_protocol_revision,current:current_protocol_revision};
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct ProtocolHello{pub range:ProtocolRange,pub capabilities:u64,pub required_capabilities:u64}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum IncompatibilityReason{RevisionMismatch,RequiredCapabilityMissing}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct ProtocolIncompatibility{pub reason:IncompatibilityReason,pub client_range:ProtocolRange,pub host_range:ProtocolRange,pub missing_capabilities:u64}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct NegotiatedProtocol{pub revision:u16,pub capabilities:u64}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Negotiation{Compatible(NegotiatedProtocol),Incompatible(ProtocolIncompatibility)}
pub fn negotiate_protocol(c:ProtocolHello,h:ProtocolHello)->Result<Negotiation>{let missing=h.required_capabilities&!c.capabilities|c.required_capabilities&!h.capabilities;if missing!=0{return Ok(Negotiation::Incompatible(ProtocolIncompatibility{reason:IncompatibilityReason::RequiredCapabilityMissing,client_range:c.range,host_range:h.range,missing_capabilities:missing}))}let min=c.range.minimum.max(h.range.minimum);let cur=c.range.current.min(h.range.current);if min>cur{Ok(Negotiation::Incompatible(ProtocolIncompatibility{reason:IncompatibilityReason::RevisionMismatch,client_range:c.range,host_range:h.range,missing_capabilities:0}))}else{Ok(Negotiation::Compatible(NegotiatedProtocol{revision:cur,capabilities:c.capabilities&h.capabilities&known_protocol_capabilities}))}}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct CorrelationId{pub value:u64}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum HostEvent{Lifecycle,Output,Monitor,ScreenRecovery,AuthorityRevoked}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum MessageKind{Hello,Request(Action),Response(Action),Cancel}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub struct MessageEnvelope{pub revision:u16,pub kind:MessageKind,pub required_capabilities:u64,pub correlation_id:Option<CorrelationId>,pub payload_len:u32}
impl MessageEnvelope{pub fn validate(self,n:usize)->Result<()>{let min=if self.kind==MessageKind::Hello{oldest_hello_revision}else{previous_protocol_revision};if self.revision<min||self.revision>current_protocol_revision{return Err(Error::UnsupportedProtocolRevision)}if self.required_capabilities&!known_protocol_capabilities!=0{return Err(Error::UnknownRequiredCapability)}if n>max_host_frame_bytes||self.payload_len as usize!=n{return Err(Error::InvalidHostFrame)}match self.kind{MessageKind::Hello if self.correlation_id.is_some()=>Err(Error::UnexpectedCorrelationId),MessageKind::Request(_)|MessageKind::Response(_)|MessageKind::Cancel if self.correlation_id.is_none()=>Err(Error::MissingCorrelationId),_=>Ok(())}}}
pub fn validate_session_id(id:&[u8])->Result<()>{session_layout::validate_session_id(id).map_err(|_|Error::InvalidSessionId)}
fn valid_bounded_text(t:&[u8],m:usize)->bool{!t.is_empty()&&t.len()<=m&&!t.contains(&0)}
pub fn is_absolute(p:&[u8])->bool{std::str::from_utf8(p).ok().is_some_and(|s|Path::new(s).is_absolute())}

#[cfg(test)] mod tests{use super::*;#[test]fn lifecycle(){assert_eq!(transition_lifecycle(Lifecycle::Starting,LifecycleEvent::ChildStarted),Ok(Lifecycle::Running));assert!(transition_lifecycle(Lifecycle::Closed,LifecycleEvent::Close).is_err())}#[test]fn session_ids(){assert!(validate_session_id(b"terminal-1").is_ok());assert!(validate_session_id(b"../x").is_err())}#[test]fn dimensions(){assert!(Dimensions{rows:1,columns:1}.validate().is_ok());assert!(Dimensions{rows:0,columns:1}.validate().is_err())}#[test]fn negotiations(){let h=ProtocolHello{range:local_protocol_range,capabilities:known_protocol_capabilities,required_capabilities:0};assert!(matches!(negotiate_protocol(h,h),Ok(Negotiation::Compatible(_))))}}
