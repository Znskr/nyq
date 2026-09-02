//! Streamable HTTP transport for modern MCP.
//!
//! Ported 1:1 from `zig-reference/src/core/mcp/streamable_http.zig`. HTTP+SSE
//! uses `std::net::TcpStream` and manual HTTP/1.1 framing (no crate deps).

use crate::core::mcp::json_value::{self, JsonMap, Value};
use crate::core::mcp::mcp_auth::{self, HttpResponseHead};
use crate::core::mcp::mcp_contract::{
    McpHttpHeader, NotificationSink, Progress, ProgressSink, TransportPrecommit,
};
use crate::core::mcp::protocol_negotiation;
use crate::core::shared::io;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[allow(non_upper_case_globals)]
pub const protocol_version: &str = protocol_negotiation::modern_protocol_version;
const MAX_SSE_EVENTS: usize = 1024;
const MAX_TOOL_SCHEMA_DEPTH: usize = 64;
const MAX_JSON_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const LEGACY_SESSIONLESS_INVALID_REQUEST_CODE: i64 = -32004;
const LEGACY_SESSION_REQUIRED_CODE: i64 = -32000;
const LEGACY_MISSING_SESSION_HEADER_CODE: i64 = -32600;
const TRANSFER_BUFFER_LEN: usize = 16 * 1024;
const HTTP_HEAD_MAX_BYTES: usize = 64 * 1024;

fn json_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(n) => Some(*n),
        Value::NumberString(lexeme) => {
            if lexeme.contains('.') || lexeme.contains('e') || lexeme.contains('E') {
                None
            } else {
                lexeme.parse().ok()
            }
        }
        _ => None,
    }
}

fn json_number_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(n) => Some(*n as f64),
        Value::Float(n) => Some(*n),
        Value::NumberString(lexeme) => lexeme.parse().ok(),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CancellationSources<'a> {
    pub caller: Option<&'a AtomicBool>,
    pub runtime: Option<&'a AtomicBool>,
}

impl CancellationSources<'_> {
    pub fn cancelled(self) -> bool {
        self.caller
            .is_some_and(|flag| flag.load(Ordering::Acquire))
            || self
                .runtime
                .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Control<'a> {
    pub deadline: i64,
    pub cancel_flag: Option<&'a AtomicBool>,
    pub lifecycle_cancel_flag: Option<&'a AtomicBool>,
}

impl Control<'_> {
    pub fn cancellation(&self) -> CancellationSources<'_> {
        CancellationSources {
            caller: self.cancel_flag,
            runtime: self.lifecycle_cancel_flag,
        }
    }
}

pub struct PostOptions<'a> {
    pub url: &'a str,
    pub static_headers: &'a [McpHttpHeader],
    pub request_body: &'a str,
    pub input_schema_json: Option<&'a str>,
    pub max_response_bytes: usize,
    pub max_event_bytes: usize,
    pub allow_version_error: bool,
    pub allow_discovery_mismatch_status: bool,
    pub progress: Option<ProgressSink<'a>>,
    pub notifications: Option<NotificationSink<'a>>,
    pub long_lived: bool,
    pub precommit: Option<&'a mut TransportPrecommit<'a>>,
    pub control: Control<'a>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostResponse {
    pub body: Vec<u8>,
    pub version_error: bool,
    pub discovery_mismatch_status: bool,
    pub auth_rejection: AuthRejection,
    pub www_authenticate: Option<Vec<u8>>,
}

impl PostResponse {
    pub fn deinit(&mut self) {
        self.body.clear();
        self.www_authenticate = None;
        self.version_error = false;
        self.discovery_mismatch_status = false;
        self.auth_rejection = AuthRejection::None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthRejection {
    None,
    Unauthorized,
    InsufficientScope,
}

impl AuthRejection {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Unauthorized => "unauthorized",
            Self::InsufficientScope => "insufficient_scope",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointError {
    InvalidEndpoint,
    InsecureEndpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderError {
    InvalidHeaderName,
    InvalidHeaderValue,
    DuplicateHeader,
    ReservedHeader,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidEndpoint,
    InsecureEndpoint,
    InvalidHeaderName,
    InvalidHeaderValue,
    DuplicateHeader,
    ReservedHeader,
    InvalidResponseLimit,
    Cancelled,
    McpRequestTimedOut,
    InvalidModernRequest,
    MissingToolInputSchema,
    UnexpectedToolInputSchema,
    InvalidToolInputSchema,
    InvalidToolArguments,
    InvalidProjectedHeaderValue,
    ToolInputSchemaTooDeep,
    InvalidHeaderAnnotationPlacement,
    InvalidHeaderAnnotationName,
    InvalidHeaderAnnotationType,
    DuplicateHeaderAnnotation,
    RedirectNotAllowed,
    UnexpectedHttpStatus,
    UnsupportedContentEncoding,
    MissingContentType,
    UnsupportedContentType,
    ResponseTooLarge,
    InvalidJsonResponse,
    MismatchedResponseId,
    InvalidSseEvent,
    UnsupportedServerRequest,
    BrokenSseStream,
    TooManySseEvents,
    SseEventTooLarge,
    MissingFinalResponse,
    ReadFailed,
    EndOfStream,
}

impl From<EndpointError> for Error {
    fn from(err: EndpointError) -> Self {
        match err {
            EndpointError::InvalidEndpoint => Error::InvalidEndpoint,
            EndpointError::InsecureEndpoint => Error::InsecureEndpoint,
        }
    }
}

impl From<HeaderError> for Error {
    fn from(err: HeaderError) -> Self {
        match err {
            HeaderError::InvalidHeaderName => Error::InvalidHeaderName,
            HeaderError::InvalidHeaderValue => Error::InvalidHeaderValue,
            HeaderError::DuplicateHeader => Error::DuplicateHeader,
            HeaderError::ReservedHeader => Error::ReservedHeader,
        }
    }
}

struct ParsedUri<'a> {
    scheme: &'a str,
    user: Option<&'a str>,
    password: Option<&'a str>,
    host: Option<&'a str>,
    port: Option<u16>,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

fn parse_uri(input: &str) -> Result<ParsedUri<'_>, EndpointError> {
    let scheme_end = input
        .find("://")
        .ok_or(EndpointError::InvalidEndpoint)?;
    let scheme = &input[..scheme_end];
    if scheme.is_empty() {
        return Err(EndpointError::InvalidEndpoint);
    }
    let rest = &input[scheme_end + 3..];
    let (rest, fragment) = match rest.find('#') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    };
    let (authority_and_path, query) = match rest.find('?') {
        Some(index) => (&rest[..index], Some(&rest[index + 1..])),
        None => (rest, None),
    };
    let (authority, path) = match authority_and_path.find('/') {
        Some(index) => (&authority_and_path[..index], &authority_and_path[index..]),
        None => (authority_and_path, ""),
    };
    let (user, password, hostport) = if let Some(at) = authority.rfind('@') {
        let userinfo = &authority[..at];
        let hostport = &authority[at + 1..];
        if let Some(colon) = userinfo.find(':') {
            (
                Some(&userinfo[..colon]),
                Some(&userinfo[colon + 1..]),
                hostport,
            )
        } else {
            (Some(userinfo), None, hostport)
        }
    } else {
        (None, None, authority)
    };
    let (host, port) = if hostport.starts_with('[') {
        let end = hostport
            .find(']')
            .ok_or(EndpointError::InvalidEndpoint)?;
        let host = &hostport[..=end];
        let after = &hostport[end + 1..];
        if after.is_empty() {
            (Some(host), None)
        } else {
            let port = after
                .strip_prefix(':')
                .ok_or(EndpointError::InvalidEndpoint)?;
            (
                Some(host),
                Some(
                    port.parse()
                        .map_err(|_| EndpointError::InvalidEndpoint)?,
                ),
            )
        }
    } else if let Some(colon) = hostport.rfind(':') {
        let host = &hostport[..colon];
        let port_str = &hostport[colon + 1..];
        if !port_str.is_empty() && port_str.bytes().all(|b| b.is_ascii_digit()) {
            (
                Some(host),
                Some(
                    port_str
                        .parse()
                        .map_err(|_| EndpointError::InvalidEndpoint)?,
                ),
            )
        } else {
            (Some(hostport), None)
        }
    } else {
        (Some(hostport), None)
    };
    if host.map(str::is_empty).unwrap_or(true) {
        return Err(EndpointError::InvalidEndpoint);
    }
    Ok(ParsedUri {
        scheme,
        user,
        password,
        host,
        port,
        path,
        query,
        fragment,
    })
}

pub fn validate_endpoint(url: &str) -> Result<(), EndpointError> {
    let uri = parse_uri(url)?;
    if uri.user.is_some() || uri.password.is_some() || uri.fragment.is_some() {
        return Err(EndpointError::InvalidEndpoint);
    }
    let host = uri.host.ok_or(EndpointError::InvalidEndpoint)?;

    if uri.scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if !uri.scheme.eq_ignore_ascii_case("http") || uri.port.is_none() {
        return Err(EndpointError::InsecureEndpoint);
    }

    if host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost") || host == "[::1]" {
        return Ok(());
    }
    Err(EndpointError::InsecureEndpoint)
}

pub fn validate_static_headers(headers: &[McpHttpHeader]) -> Result<(), HeaderError> {
    for (index, header) in headers.iter().enumerate() {
        if !is_http_token(&header.name) {
            return Err(HeaderError::InvalidHeaderName);
        }
        validate_header_value(&header.value)?;
        if is_reserved_header(&header.name) {
            return Err(HeaderError::ReservedHeader);
        }
        for previous in &headers[..index] {
            if previous.name.eq_ignore_ascii_case(&header.name) {
                return Err(HeaderError::DuplicateHeader);
            }
        }
    }
    Ok(())
}

pub fn validate_header_value(value: &str) -> Result<(), HeaderError> {
    if !is_valid_header_value(value.as_bytes()) {
        return Err(HeaderError::InvalidHeaderValue);
    }
    Ok(())
}

fn is_http_token(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() {
            continue;
        }
        match byte {
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^'
            | b'_' | b'`' | b'|' | b'~' => {}
            _ => return false,
        }
    }
    true
}

fn is_valid_header_value(value: &[u8]) -> bool {
    for &byte in value {
        if (byte < 0x20 && byte != b'\t') || byte == 0x7f {
            return false;
        }
    }
    true
}

fn is_reserved_header(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "accept",
        "accept-encoding",
        "connection",
        "content-length",
        "content-type",
        "host",
        "last-event-id",
        "mcp-method",
        "mcp-name",
        "mcp-protocol-version",
        "mcp-session-id",
        "transfer-encoding",
    ];
    for candidate in RESERVED {
        if name.eq_ignore_ascii_case(candidate) {
            return true;
        }
    }
    starts_with_ignore_case(name, "mcp-param-")
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

trait IoRead {
    fn buffered(&self) -> &[u8];
    fn toss(&mut self, n: usize);
    fn fill_more(&mut self) -> Result<(), Error>;
    fn stream(&mut self, dest: &mut [u8]) -> Result<usize, Error>;

    fn read_slice_short(&mut self, dest: &mut [u8]) -> Result<usize, Error> {
        let mut written = 0;
        while written < dest.len() {
            let take = {
                let buffered = self.buffered();
                if buffered.is_empty() {
                    0
                } else {
                    let take = buffered.len().min(dest.len() - written);
                    dest[written..written + take].copy_from_slice(&buffered[..take]);
                    take
                }
            };
            if take == 0 {
                match self.fill_more() {
                    Ok(()) => continue,
                    Err(Error::EndOfStream) => return Ok(written),
                    Err(err) => return Err(err),
                }
            }
            self.toss(take);
            written += take;
        }
        Ok(written)
    }

    fn read_slice_all(&mut self, dest: &mut [u8]) -> Result<(), Error> {
        let n = self.read_slice_short(dest)?;
        if n < dest.len() {
            Err(Error::EndOfStream)
        } else {
            Ok(())
        }
    }

    fn discard_all(&mut self, mut n: usize) -> Result<(), Error> {
        while n > 0 {
            let take = {
                let buffered = self.buffered();
                if buffered.is_empty() {
                    0
                } else {
                    buffered.len().min(n)
                }
            };
            if take == 0 {
                self.fill_more()?;
                continue;
            }
            self.toss(take);
            n -= take;
        }
        Ok(())
    }
}

#[cfg(test)]
struct SliceReader<'a> {
    data: &'a [u8],
    seek: usize,
}

#[cfg(test)]
impl<'a> SliceReader<'a> {
    fn fixed(data: &'a [u8]) -> Self {
        Self { data, seek: 0 }
    }
}

#[cfg(test)]
impl IoRead for SliceReader<'_> {
    fn buffered(&self) -> &[u8] {
        &self.data[self.seek..]
    }

    fn toss(&mut self, n: usize) {
        self.seek = (self.seek + n).min(self.data.len());
    }

    fn fill_more(&mut self) -> Result<(), Error> {
        if self.seek >= self.data.len() {
            Err(Error::EndOfStream)
        } else {
            Ok(())
        }
    }

    fn stream(&mut self, dest: &mut [u8]) -> Result<usize, Error> {
        let available = self.buffered();
        if available.is_empty() {
            return Err(Error::EndOfStream);
        }
        let n = available.len().min(dest.len());
        dest[..n].copy_from_slice(&available[..n]);
        self.toss(n);
        Ok(n)
    }
}

struct McpContentLengthReader<'buf, 'src> {
    interface: ContentLengthInterface<'buf, 'src>,
}

struct ContentLengthInterface<'buf, 'src> {
    source: &'src mut dyn IoRead,
    remaining: u64,
    buffer: &'buf mut [u8],
    seek: usize,
    end: usize,
}

impl<'buf, 'src> McpContentLengthReader<'buf, 'src> {
    fn init(
        source: &'src mut dyn IoRead,
        content_length: u64,
        buffer: &'buf mut [u8],
    ) -> Self {
        Self {
            interface: ContentLengthInterface {
                source,
                remaining: content_length,
                buffer,
                seek: 0,
                end: 0,
            },
        }
    }
}

impl ContentLengthInterface<'_, '_> {
    fn compact(&mut self) {
        if self.seek == 0 {
            return;
        }
        let remaining = self.end.saturating_sub(self.seek);
        if remaining > 0 {
            self.buffer.copy_within(self.seek..self.end, 0);
        }
        self.seek = 0;
        self.end = remaining;
    }

    fn pull_into(&mut self, dest: &mut [u8]) -> Result<usize, Error> {
        if self.remaining == 0 {
            return Err(Error::EndOfStream);
        }
        if dest.is_empty() {
            return Ok(0);
        }
        let cap = (dest.len() as u64).min(self.remaining) as usize;
        let n = self.source.stream(&mut dest[..cap])?;
        if n as u64 > self.remaining {
            return Err(Error::ReadFailed);
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

impl IoRead for ContentLengthInterface<'_, '_> {
    fn buffered(&self) -> &[u8] {
        &self.buffer[self.seek..self.end]
    }

    fn toss(&mut self, n: usize) {
        self.seek = (self.seek + n).min(self.end);
    }

    fn fill_more(&mut self) -> Result<(), Error> {
        self.compact();
        if self.end >= self.buffer.len() {
            return if self.end > self.seek {
                Ok(())
            } else {
                Err(Error::EndOfStream)
            };
        }
        if self.remaining == 0 {
            return Err(Error::EndOfStream);
        }
        let dest_start = self.end;
        let dest_len = self.buffer.len() - dest_start;
        let cap = (dest_len as u64).min(self.remaining) as usize;
        let n = self.source.stream(&mut self.buffer[dest_start..dest_start + cap])?;
        if n as u64 > self.remaining {
            return Err(Error::ReadFailed);
        }
        self.remaining -= n as u64;
        if n == 0 {
            return Err(Error::EndOfStream);
        }
        self.end += n;
        Ok(())
    }

    fn stream(&mut self, dest: &mut [u8]) -> Result<usize, Error> {
        let buffered_len = self.end.saturating_sub(self.seek);
        if buffered_len > 0 {
            let n = buffered_len.min(dest.len());
            dest[..n].copy_from_slice(&self.buffer[self.seek..self.seek + n]);
            self.toss(n);
            return Ok(n);
        }
        self.pull_into(dest)
    }
}

struct TcpReader {
    stream: TcpStream,
    buffer: Vec<u8>,
    seek: usize,
    end: usize,
}

impl TcpReader {
    fn new(stream: TcpStream, leftover: Vec<u8>) -> Self {
        let end = leftover.len();
        let mut buffer = leftover;
        if buffer.len() < TRANSFER_BUFFER_LEN {
            buffer.resize(TRANSFER_BUFFER_LEN, 0);
        }
        Self {
            stream,
            buffer,
            seek: 0,
            end,
        }
    }

    fn compact(&mut self) {
        if self.seek == 0 {
            return;
        }
        let remaining = self.end.saturating_sub(self.seek);
        if remaining > 0 {
            self.buffer.copy_within(self.seek..self.end, 0);
        }
        self.seek = 0;
        self.end = remaining;
    }
}

impl IoRead for TcpReader {
    fn buffered(&self) -> &[u8] {
        &self.buffer[self.seek..self.end]
    }

    fn toss(&mut self, n: usize) {
        self.seek = (self.seek + n).min(self.end);
    }

    fn fill_more(&mut self) -> Result<(), Error> {
        self.compact();
        if self.end >= self.buffer.len() {
            return if self.end > self.seek {
                Ok(())
            } else {
                Err(Error::EndOfStream)
            };
        }
        let n = self
            .stream
            .read(&mut self.buffer[self.end..])
            .map_err(|_| Error::ReadFailed)?;
        if n == 0 {
            return Err(Error::EndOfStream);
        }
        self.end += n;
        Ok(())
    }

    fn stream(&mut self, dest: &mut [u8]) -> Result<usize, Error> {
        let buffered_len = self.end.saturating_sub(self.seek);
        if buffered_len > 0 {
            let n = buffered_len.min(dest.len());
            dest[..n].copy_from_slice(&self.buffer[self.seek..self.seek + n]);
            self.toss(n);
            return Ok(n);
        }
        let n = self.stream.read(dest).map_err(|_| Error::ReadFailed)?;
        if n == 0 {
            Err(Error::EndOfStream)
        } else {
            Ok(n)
        }
    }
}

struct ChunkedReader<'buf, 'src> {
    source: &'src mut dyn IoRead,
    buffer: &'buf mut [u8],
    seek: usize,
    end: usize,
    chunk_remaining: u64,
    need_chunk_crlf: bool,
    finished: bool,
}

impl<'buf, 'src> ChunkedReader<'buf, 'src> {
    fn init(source: &'src mut dyn IoRead, buffer: &'buf mut [u8]) -> Self {
        Self {
            source,
            buffer,
            seek: 0,
            end: 0,
            chunk_remaining: 0,
            need_chunk_crlf: false,
            finished: false,
        }
    }

    fn compact(&mut self) {
        if self.seek == 0 {
            return;
        }
        let remaining = self.end.saturating_sub(self.seek);
        if remaining > 0 {
            self.buffer.copy_within(self.seek..self.end, 0);
        }
        self.seek = 0;
        self.end = remaining;
    }

    fn read_line(source: &mut dyn IoRead) -> Result<String, Error> {
        let mut line = Vec::new();
        loop {
            if source.buffered().is_empty() {
                source.fill_more()?;
            }
            let (copy, toss_n, found) = {
                let buffered = source.buffered();
                if let Some(pos) = buffered.iter().position(|&b| b == b'\n') {
                    (buffered[..pos].to_vec(), pos + 1, true)
                } else {
                    (buffered.to_vec(), buffered.len(), false)
                }
            };
            source.toss(toss_n);
            line.extend_from_slice(&copy);
            if found {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return String::from_utf8(line).map_err(|_| Error::ReadFailed);
            }
            if line.len() > 4096 {
                return Err(Error::ReadFailed);
            }
        }
    }

    fn consume_crlf(source: &mut dyn IoRead) -> Result<(), Error> {
        let mut crlf = [0u8; 2];
        source.read_slice_all(&mut crlf)?;
        if &crlf != b"\r\n" {
            return Err(Error::ReadFailed);
        }
        Ok(())
    }

    fn start_next_chunk(&mut self) -> Result<(), Error> {
        if self.need_chunk_crlf {
            Self::consume_crlf(self.source)?;
            self.need_chunk_crlf = false;
        }
        let line = Self::read_line(self.source)?;
        let size_token = line.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_token, 16).map_err(|_| Error::ReadFailed)?;
        if size == 0 {
            loop {
                let trailer = Self::read_line(self.source)?;
                if trailer.is_empty() {
                    break;
                }
            }
            self.finished = true;
            return Err(Error::EndOfStream);
        }
        self.chunk_remaining = size;
        Ok(())
    }
}

impl IoRead for ChunkedReader<'_, '_> {
    fn buffered(&self) -> &[u8] {
        &self.buffer[self.seek..self.end]
    }

    fn toss(&mut self, n: usize) {
        self.seek = (self.seek + n).min(self.end);
    }

    fn fill_more(&mut self) -> Result<(), Error> {
        self.compact();
        if self.end >= self.buffer.len() {
            return if self.end > self.seek {
                Ok(())
            } else {
                Err(Error::EndOfStream)
            };
        }
        if self.finished {
            return Err(Error::EndOfStream);
        }
        if self.chunk_remaining == 0 {
            self.start_next_chunk()?;
        }
        let dest_start = self.end;
        let dest_len = self.buffer.len() - dest_start;
        let cap = (dest_len as u64).min(self.chunk_remaining) as usize;
        let n = self.source.stream(&mut self.buffer[dest_start..dest_start + cap])?;
        if n as u64 > self.chunk_remaining {
            return Err(Error::ReadFailed);
        }
        self.chunk_remaining -= n as u64;
        if self.chunk_remaining == 0 {
            self.need_chunk_crlf = true;
        }
        if n == 0 {
            return Err(Error::EndOfStream);
        }
        self.end += n;
        Ok(())
    }

    fn stream(&mut self, dest: &mut [u8]) -> Result<usize, Error> {
        let buffered_len = self.end.saturating_sub(self.seek);
        if buffered_len > 0 {
            let n = buffered_len.min(dest.len());
            dest[..n].copy_from_slice(&self.buffer[self.seek..self.seek + n]);
            self.toss(n);
            return Ok(n);
        }
        if self.finished {
            return Err(Error::EndOfStream);
        }
        if self.chunk_remaining == 0 {
            self.start_next_chunk()?;
        }
        let cap = (dest.len() as u64).min(self.chunk_remaining) as usize;
        let n = self.source.stream(&mut dest[..cap])?;
        if n as u64 > self.chunk_remaining {
            return Err(Error::ReadFailed);
        }
        self.chunk_remaining -= n as u64;
        if self.chunk_remaining == 0 {
            self.need_chunk_crlf = true;
        }
        if n == 0 {
            Err(Error::EndOfStream)
        } else {
            Ok(n)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum McpResponseBodyFraming {
    ContentLength,
    Standard,
}

impl McpResponseBodyFraming {
    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            Self::ContentLength => "content_length",
            Self::Standard => "standard",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferEncoding {
    None,
    Chunked,
}

fn mcp_response_body_framing(
    transfer_encoding: TransferEncoding,
    content_length: Option<u64>,
) -> McpResponseBodyFraming {
    if transfer_encoding == TransferEncoding::None && content_length.is_some() {
        McpResponseBodyFraming::ContentLength
    } else {
        McpResponseBodyFraming::Standard
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaType {
    Json,
    Sse,
}

impl MediaType {
    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Sse => "sse",
        }
    }
}

fn parse_media_type(value: &str) -> Option<MediaType> {
    let separator = value.find(';').unwrap_or(value.len());
    let media_type = value[..separator].trim_matches(|c: char| matches!(c, ' ' | '\t'));
    if media_type.eq_ignore_ascii_case("application/json") {
        Some(MediaType::Json)
    } else if media_type.eq_ignore_ascii_case("text/event-stream") {
        Some(MediaType::Sse)
    } else {
        None
    }
}

fn is_plain_text_media_type(value: &str) -> bool {
    let separator = value.find(';').unwrap_or(value.len());
    let media_type = value[..separator].trim_matches(|c: char| matches!(c, ' ' | '\t'));
    media_type.eq_ignore_ascii_case("text/plain")
}

pub fn validate_tool_input_schema_headers(schema_json: &str) -> Result<(), Error> {
    let parsed =
        json_value::parse_json(schema_json).map_err(|_| Error::InvalidToolInputSchema)?;
    validate_tool_input_schema_value(&parsed)
}

pub fn post(mut options: PostOptions<'_>) -> Result<PostResponse, Error> {
    validate_endpoint(options.url)?;
    validate_static_headers(options.static_headers)?;
    if options.max_response_bytes == 0 || options.max_event_bytes == 0 {
        return Err(Error::InvalidResponseLimit);
    }

    let cancellation = options.control.cancellation();
    if cancellation.cancelled() {
        return Err(Error::Cancelled);
    }
    if io::milli_timestamp() >= options.control.deadline {
        return Err(Error::McpRequestTimedOut);
    }

    post_core(&mut options)
}

struct HttpHeader {
    name: String,
    value: String,
}

struct OwnedHeaders {
    items: Vec<HttpHeader>,
}

impl OwnedHeaders {
    fn append(&mut self, name: &str, value: &str) {
        self.items.push(HttpHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
    }
}

struct PreparedRequest {
    headers: OwnedHeaders,
    id: i64,
    is_discovery: bool,
}

struct RequestInfo {
    id: i64,
    method: String,
    name: Option<String>,
}

fn post_core(options: &mut PostOptions<'_>) -> Result<PostResponse, Error> {
    let prepared = prepare_request(
        options.static_headers,
        options.request_body,
        options.input_schema_json,
    )?;
    let uri = parse_uri(options.url).map_err(|_| Error::InvalidEndpoint)?;
    let host = uri.host.ok_or(Error::InvalidEndpoint)?;
    let default_port: u16 = if uri.scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    let port = uri.port.unwrap_or(default_port);
    let path = if uri.path.is_empty() { "/" } else { uri.path };
    let path_and_query = match uri.query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    let host_header = if uri.port.is_some() && uri.port != Some(default_port) {
        format!("{host}:{port}")
    } else if uri.port.is_some() {
        format!("{host}:{port}")
    } else {
        host.to_string()
    };

    let mut stream =
        TcpStream::connect(format!("{host}:{port}")).map_err(|_| Error::ReadFailed)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|_| Error::ReadFailed)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(60)))
        .map_err(|_| Error::ReadFailed)?;
    stream.set_nodelay(true).ok();

    let mut request = String::new();
    request.push_str(&format!("POST {path_and_query} HTTP/1.1\r\n"));
    request.push_str(&format!("Host: {host_header}\r\n"));
    request.push_str("Content-Type: application/json\r\n");
    request.push_str(&format!(
        "Content-Length: {}\r\n",
        options.request_body.len()
    ));
    request.push_str("Connection: close\r\n");
    for header in &prepared.headers.items {
        request.push_str(&format!("{}: {}\r\n", header.name, header.value));
    }
    request.push_str("\r\n");

    let mut acquired = false;
    if let Some(precommit) = options.precommit.as_mut() {
        precommit.acquire().map_err(|_| Error::ReadFailed)?;
        acquired = true;
    }
    let send_result = (|| {
        stream
            .write_all(request.as_bytes())
            .map_err(|_| Error::ReadFailed)?;
        stream
            .write_all(options.request_body.as_bytes())
            .map_err(|_| Error::ReadFailed)?;
        stream.flush().map_err(|_| Error::ReadFailed)?;
        Ok::<(), Error>(())
    })();
    if acquired {
        if let Some(precommit) = options.precommit.as_mut() {
            precommit.release();
        }
    }
    send_result?;

    let (head_raw, leftover) = read_http_head(&mut stream)?;
    let parsed_head = parse_response_head(&head_raw)?;
    let status = parsed_head.status;
    if (300..400).contains(&status) {
        return Err(Error::RedirectNotAllowed);
    }

    if status == 401 || status == 403 {
        let http_head = HttpResponseHead::parse(&format!("{head_raw}\r\n\r\n"))
            .map_err(|_| Error::ReadFailed)?;
        let authenticate = mcp_auth::collect_authenticate_header(&http_head);
        return Ok(PostResponse {
            body: Vec::new(),
            version_error: false,
            discovery_mismatch_status: false,
            auth_rejection: if status == 401 {
                AuthRejection::Unauthorized
            } else {
                AuthRejection::InsufficientScope
            },
            www_authenticate: authenticate.map(|value| value.into_bytes()),
        });
    }

    let version_error = options.allow_version_error && status == 400;
    let discovery_mismatch_status = options.allow_discovery_mismatch_status
        && (status == 404 || status == 405);
    if status != 200 && !version_error && !discovery_mismatch_status {
        return Err(Error::UnexpectedHttpStatus);
    }
    if !parsed_head.content_encoding_identity {
        return Err(Error::UnsupportedContentEncoding);
    }
    if discovery_mismatch_status {
        return Ok(PostResponse {
            body: Vec::new(),
            version_error: false,
            discovery_mismatch_status: true,
            auth_rejection: AuthRejection::None,
            www_authenticate: None,
        });
    }

    let content_type = parsed_head
        .content_type
        .as_deref()
        .ok_or(Error::MissingContentType)?;
    let media_type = parse_media_type(content_type);
    let legacy_plain_text_discovery =
        version_error && prepared.is_discovery && is_plain_text_media_type(content_type);
    if media_type.is_none() && !legacy_plain_text_discovery {
        return Err(Error::UnsupportedContentType);
    }
    if version_error && media_type != Some(MediaType::Json) && !legacy_plain_text_discovery {
        return Err(Error::UnsupportedContentType);
    }

    let mut tcp_reader = TcpReader::new(stream, leftover);
    let framing = mcp_response_body_framing(
        parsed_head.transfer_encoding,
        parsed_head.content_length,
    );

    let (body, body_auth_rejection) = match framing {
        McpResponseBodyFraming::ContentLength => {
            let mut transfer_buffer = [0u8; TRANSFER_BUFFER_LEN];
            let mut content_length_reader = McpContentLengthReader::init(
                &mut tcp_reader,
                parsed_head.content_length.unwrap_or(0),
                &mut transfer_buffer,
            );
            read_response_body(
                &mut content_length_reader.interface,
                media_type,
                legacy_plain_text_discovery,
                prepared.id,
                prepared.is_discovery,
                options,
            )?
        }
        McpResponseBodyFraming::Standard => {
            if parsed_head.transfer_encoding == TransferEncoding::Chunked {
                let mut transfer_buffer = [0u8; TRANSFER_BUFFER_LEN];
                let mut chunked =
                    ChunkedReader::init(&mut tcp_reader, &mut transfer_buffer);
                read_response_body(
                    &mut chunked,
                    media_type,
                    legacy_plain_text_discovery,
                    prepared.id,
                    prepared.is_discovery,
                    options,
                )?
            } else {
                read_response_body(
                    &mut tcp_reader,
                    media_type,
                    legacy_plain_text_discovery,
                    prepared.id,
                    prepared.is_discovery,
                    options,
                )?
            }
        }
    };

    if legacy_plain_text_discovery
        && body_auth_rejection == AuthRejection::None
        && !is_legacy_discovery_compatibility_response(&body)?
    {
        return Err(Error::UnsupportedContentType);
    }
    Ok(PostResponse {
        body,
        version_error,
        discovery_mismatch_status,
        auth_rejection: body_auth_rejection,
        www_authenticate: None,
    })
}

struct ParsedResponseHead {
    status: u16,
    content_type: Option<String>,
    content_length: Option<u64>,
    transfer_encoding: TransferEncoding,
    content_encoding_identity: bool,
}

fn read_http_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>), Error> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        if buf.len() > HTTP_HEAD_MAX_BYTES {
            return Err(Error::ReadFailed);
        }
        let n = stream.read(&mut tmp).map_err(|_| Error::ReadFailed)?;
        if n == 0 {
            return Err(Error::ReadFailed);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            let head = std::str::from_utf8(&buf[..pos]).map_err(|_| Error::ReadFailed)?;
            let leftover = buf[pos + 4..].to_vec();
            return Ok((head.to_string(), leftover));
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_response_head(head: &str) -> Result<ParsedResponseHead, Error> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(Error::ReadFailed)?;
    let mut status_parts = status_line.split_whitespace();
    status_parts.next().ok_or(Error::ReadFailed)?;
    let status: u16 = status_parts
        .next()
        .ok_or(Error::ReadFailed)?
        .parse()
        .map_err(|_| Error::ReadFailed)?;

    let mut content_type = None;
    let mut content_length = None;
    let mut transfer_encoding = TransferEncoding::None;
    let mut content_encoding_identity = true;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(Error::ReadFailed)?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse().map_err(|_| Error::ReadFailed)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if value.eq_ignore_ascii_case("chunked")
                || value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case("chunked"))
            {
                transfer_encoding = TransferEncoding::Chunked;
            }
        } else if name.eq_ignore_ascii_case("content-encoding") {
            let encoding = value.trim();
            if !encoding.is_empty() && !encoding.eq_ignore_ascii_case("identity") {
                content_encoding_identity = false;
            }
        }
    }
    Ok(ParsedResponseHead {
        status,
        content_type,
        content_length,
        transfer_encoding,
        content_encoding_identity,
    })
}

fn read_response_body(
    reader: &mut dyn IoRead,
    media_type: Option<MediaType>,
    legacy_plain_text_discovery: bool,
    request_id: i64,
    is_discovery: bool,
    options: &mut PostOptions<'_>,
) -> Result<(Vec<u8>, AuthRejection), Error> {
    let mut body_auth_rejection = AuthRejection::None;
    let body = if legacy_plain_text_discovery {
        read_json_response_classified(
            reader,
            request_id,
            options.max_response_bytes,
            is_discovery,
            &mut body_auth_rejection,
        )?
    } else {
        match media_type.ok_or(Error::UnsupportedContentType)? {
            MediaType::Json => read_json_response_classified(
                reader,
                request_id,
                options.max_response_bytes,
                is_discovery,
                &mut body_auth_rejection,
            )?,
            MediaType::Sse => {
                let max_response_bytes = if options.long_lived {
                    usize::MAX
                } else {
                    options.max_response_bytes
                };
                let max_events = if options.long_lived {
                    usize::MAX
                } else {
                    MAX_SSE_EVENTS
                };
                let cancellation = options.control.cancellation();
                read_sse_response(
                    reader,
                    request_id,
                    max_response_bytes,
                    options.max_event_bytes,
                    max_events,
                    options.progress.as_mut(),
                    options.notifications.as_mut(),
                    cancellation,
                )?
            }
        }
    };
    Ok((body, body_auth_rejection))
}

fn prepare_request(
    static_headers: &[McpHttpHeader],
    request_body: &str,
    input_schema_json: Option<&str>,
) -> Result<PreparedRequest, Error> {
    let parsed =
        json_value::parse_json(request_body).map_err(|_| Error::InvalidModernRequest)?;
    let request_info = modern_request_info(&parsed)?;
    let is_discovery = request_info.method == "server/discover";

    let mut headers = OwnedHeaders { items: Vec::new() };
    headers.append("Accept", "application/json, text/event-stream");
    headers.append("MCP-Protocol-Version", protocol_version);
    headers.append("Mcp-Method", &request_info.method);
    if let Some(name) = &request_info.name {
        let encoded_name = encode_header_value(name);
        headers.append("Mcp-Name", &encoded_name);
    }
    for header in static_headers {
        headers.append(&header.name, &header.value);
    }

    if request_info.method == "tools/call" {
        let schema_json = input_schema_json.ok_or(Error::MissingToolInputSchema)?;
        let params = parsed
            .as_object()
            .and_then(|object| object.get("params"))
            .and_then(Value::as_object)
            .ok_or(Error::InvalidModernRequest)?;
        append_projected_headers(&mut headers, schema_json, params)?;
    } else if input_schema_json.is_some() {
        return Err(Error::UnexpectedToolInputSchema);
    }
    Ok(PreparedRequest {
        headers,
        id: request_info.id,
        is_discovery,
    })
}

fn modern_request_info(value: &Value) -> Result<RequestInfo, Error> {
    let object = value.as_object().ok_or(Error::InvalidModernRequest)?;
    let jsonrpc = object.get("jsonrpc").ok_or(Error::InvalidModernRequest)?;
    match jsonrpc.as_str() {
        Some("2.0") => {}
        _ => return Err(Error::InvalidModernRequest),
    }
    let id_value = object.get("id").ok_or(Error::InvalidModernRequest)?;
    let id = json_integer(id_value).ok_or(Error::InvalidModernRequest)?;
    let method_value = object.get("method").ok_or(Error::InvalidModernRequest)?;
    let method = method_value
        .as_str()
        .ok_or(Error::InvalidModernRequest)?;
    if method.is_empty() {
        return Err(Error::InvalidModernRequest);
    }
    let params_value = object.get("params").ok_or(Error::InvalidModernRequest)?;
    let params = params_value
        .as_object()
        .ok_or(Error::InvalidModernRequest)?;
    let meta_value = params.get("_meta").ok_or(Error::InvalidModernRequest)?;
    let meta = meta_value.as_object().ok_or(Error::InvalidModernRequest)?;
    let version_value = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .ok_or(Error::InvalidModernRequest)?;
    match version_value.as_str() {
        Some(version) if version == protocol_version => {}
        _ => return Err(Error::InvalidModernRequest),
    }
    let client_info = meta
        .get("io.modelcontextprotocol/clientInfo")
        .ok_or(Error::InvalidModernRequest)?;
    let client_info = client_info
        .as_object()
        .ok_or(Error::InvalidModernRequest)?;
    let client_name = client_info
        .get("name")
        .ok_or(Error::InvalidModernRequest)?;
    let client_version = client_info
        .get("version")
        .ok_or(Error::InvalidModernRequest)?;
    let client_name = client_name.as_str().ok_or(Error::InvalidModernRequest)?;
    let client_version = client_version
        .as_str()
        .ok_or(Error::InvalidModernRequest)?;
    if client_name.is_empty() || client_version.is_empty() {
        return Err(Error::InvalidModernRequest);
    }
    let capabilities = meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .ok_or(Error::InvalidModernRequest)?;
    if !capabilities.is_object() {
        return Err(Error::InvalidModernRequest);
    }

    let identity_field = if method == "tools/call" || method == "prompts/get" {
        Some("name")
    } else if method == "resources/read" {
        Some("uri")
    } else {
        None
    };
    let name = if let Some(field) = identity_field {
        let name_value = params.get(field).ok_or(Error::InvalidModernRequest)?;
        let name = name_value.as_str().ok_or(Error::InvalidModernRequest)?;
        Some(name.to_string())
    } else {
        None
    };

    Ok(RequestInfo {
        id,
        method: method.to_string(),
        name,
    })
}

fn append_projected_headers(
    headers: &mut OwnedHeaders,
    schema_json: &str,
    request_params: &JsonMap,
) -> Result<(), Error> {
    let schema =
        json_value::parse_json(schema_json).map_err(|_| Error::InvalidToolInputSchema)?;
    validate_tool_input_schema_value(&schema)?;

    let arguments_value = request_params
        .get("arguments")
        .ok_or(Error::InvalidToolArguments)?;
    let arguments = arguments_value
        .as_object()
        .ok_or(Error::InvalidToolArguments)?;
    let schema_object = schema.as_object().ok_or(Error::InvalidToolInputSchema)?;
    let properties_value = match schema_object.get("properties") {
        Some(value) => value,
        None => return Ok(()),
    };
    let properties = properties_value
        .as_object()
        .ok_or(Error::InvalidToolInputSchema)?;
    append_projected_properties(headers, properties, arguments, 0)
}

fn append_projected_properties(
    headers: &mut OwnedHeaders,
    properties: &JsonMap,
    arguments: &JsonMap,
    depth: usize,
) -> Result<(), Error> {
    if depth >= MAX_TOOL_SCHEMA_DEPTH {
        return Err(Error::ToolInputSchemaTooDeep);
    }

    for (key, entry) in properties.iter() {
        let property = match entry.as_object() {
            Some(object) => object,
            None => continue,
        };
        let argument = match arguments.get(key) {
            Some(value) => value,
            None => continue,
        };
        if argument.is_null() {
            continue;
        }

        if let Some(annotation) = property.get("x-mcp-header") {
            let type_value = property
                .get("type")
                .and_then(Value::as_str)
                .ok_or(Error::InvalidProjectedHeaderValue)?;
            let plain_value = if type_value == "string" {
                let text = argument
                    .as_str()
                    .ok_or(Error::InvalidProjectedHeaderValue)?;
                text.to_string()
            } else if type_value == "integer" {
                let integer = json_integer(argument).ok_or(Error::InvalidProjectedHeaderValue)?;
                if integer < -MAX_JSON_SAFE_INTEGER || integer > MAX_JSON_SAFE_INTEGER {
                    return Err(Error::InvalidProjectedHeaderValue);
                }
                integer.to_string()
            } else {
                let flag = argument
                    .as_bool()
                    .ok_or(Error::InvalidProjectedHeaderValue)?;
                if flag { "true" } else { "false" }.to_string()
            };
            let encoded_value = encode_header_value(&plain_value);
            let annotation_name = annotation
                .as_str()
                .ok_or(Error::InvalidProjectedHeaderValue)?;
            let header_name = format!("Mcp-Param-{annotation_name}");
            headers.append(&header_name, &encoded_value);
        }

        let nested_properties = match property.get("properties") {
            Some(value) => value,
            None => continue,
        };
        let nested = nested_properties
            .as_object()
            .ok_or(Error::InvalidToolInputSchema)?;
        if let Some(argument_object) = argument.as_object() {
            append_projected_properties(headers, nested, argument_object, depth + 1)?;
        } else if contains_header_annotation(nested_properties, depth + 1)? {
            return Err(Error::InvalidProjectedHeaderValue);
        }
    }
    Ok(())
}

fn validate_tool_input_schema_value(value: &Value) -> Result<(), Error> {
    let object = value.as_object().ok_or(Error::InvalidToolInputSchema)?;
    for (key, child) in object.iter() {
        if key == "properties" {
            continue;
        }
        if key == "x-mcp-header" {
            return Err(Error::InvalidHeaderAnnotationPlacement);
        }
        if contains_header_annotation(child, 0)? {
            return Err(Error::InvalidHeaderAnnotationPlacement);
        }
    }

    let properties_value = match object.get("properties") {
        Some(value) => value,
        None => return Ok(()),
    };
    let properties = properties_value
        .as_object()
        .ok_or(Error::InvalidToolInputSchema)?;
    let mut names = Vec::new();
    validate_property_annotations(properties, &mut names, 0)
}

fn validate_property_annotations(
    properties: &JsonMap,
    names: &mut Vec<String>,
    depth: usize,
) -> Result<(), Error> {
    if depth >= MAX_TOOL_SCHEMA_DEPTH {
        return Err(Error::ToolInputSchemaTooDeep);
    }

    for (_key, entry) in properties.iter() {
        let property = match entry.as_object() {
            Some(object) => object,
            None => {
                if contains_header_annotation(entry, 0)? {
                    return Err(Error::InvalidHeaderAnnotationPlacement);
                }
                continue;
            }
        };
        if let Some(header_value) = property.get("x-mcp-header") {
            let header_name = match header_value.as_str() {
                Some(name) if is_http_token(name) => name,
                _ => return Err(Error::InvalidHeaderAnnotationName),
            };
            let type_value = property
                .get("type")
                .ok_or(Error::InvalidHeaderAnnotationType)?;
            let type_name = type_value
                .as_str()
                .ok_or(Error::InvalidHeaderAnnotationType)?;
            if type_name != "string" && type_name != "integer" && type_name != "boolean" {
                return Err(Error::InvalidHeaderAnnotationType);
            }
            if property.contains("$ref")
                || property.contains("allOf")
                || property.contains("anyOf")
                || property.contains("oneOf")
            {
                return Err(Error::InvalidHeaderAnnotationType);
            }
            for name in names.iter() {
                if name.eq_ignore_ascii_case(header_name) {
                    return Err(Error::DuplicateHeaderAnnotation);
                }
            }
            names.push(header_name.to_string());
        }

        if let Some(nested) = property.get("properties") {
            let nested_object = nested.as_object().ok_or(Error::InvalidToolInputSchema)?;
            validate_property_annotations(nested_object, names, depth + 1)?;
        }

        for (child_key, child) in property.iter() {
            if child_key == "x-mcp-header" || child_key == "properties" {
                continue;
            }
            if contains_header_annotation(child, 0)? {
                return Err(Error::InvalidHeaderAnnotationPlacement);
            }
        }
    }
    Ok(())
}

fn contains_header_annotation(value: &Value, depth: usize) -> Result<bool, Error> {
    if depth >= MAX_TOOL_SCHEMA_DEPTH {
        return Err(Error::ToolInputSchemaTooDeep);
    }
    match value {
        Value::Object(object) => {
            if object.contains("x-mcp-header") {
                return Ok(true);
            }
            for (_key, child) in object.iter() {
                if contains_header_annotation(child, depth + 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Value::Array(items) => {
            for item in items {
                if contains_header_annotation(item, depth + 1)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn encode_header_value(value: &str) -> String {
    if !needs_header_encoding(value.as_bytes()) {
        return value.to_string();
    }
    let encoded = encode_base64_standard(value.as_bytes());
    format!("=?base64?{encoded}?=")
}

fn needs_header_encoding(value: &[u8]) -> bool {
    if value.starts_with(b"=?base64?") {
        return true;
    }
    if !value.is_empty()
        && (value[0] == b' ' || value[0] == b'\t' || value[value.len() - 1] == b' ' || value[value.len() - 1] == b'\t')
    {
        return true;
    }
    for &byte in value {
        if byte < 0x20 || byte > 0x7e {
            return true;
        }
    }
    false
}

fn encode_base64_standard(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= input.len() {
        let value = (u32::from(input[index]) << 16)
            | (u32::from(input[index + 1]) << 8)
            | u32::from(input[index + 2]);
        out.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(value & 0x3f) as usize] as char);
        index += 3;
    }
    let remainder = input.len() - index;
    if remainder != 0 {
        let value = u32::from(input[index]) << 16
            | if remainder == 2 {
                u32::from(input[index + 1]) << 8
            } else {
                0
            };
        out.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        if remainder == 2 {
            out.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
            out.push('=');
        } else {
            out.push('=');
            out.push('=');
        }
    }
    out
}

fn read_json_response(
    reader: &mut dyn IoRead,
    request_id: i64,
    max_response_bytes: usize,
    allow_discovery_error_id_mismatch: bool,
) -> Result<Vec<u8>, Error> {
    let mut auth_rejection = AuthRejection::None;
    let body = read_json_response_classified(
        reader,
        request_id,
        max_response_bytes,
        allow_discovery_error_id_mismatch,
        &mut auth_rejection,
    )?;
    if auth_rejection != AuthRejection::None {
        return Err(Error::InvalidJsonResponse);
    }
    Ok(body)
}

fn read_json_response_classified(
    reader: &mut dyn IoRead,
    request_id: i64,
    max_response_bytes: usize,
    allow_discovery_error_id_mismatch: bool,
    auth_rejection: &mut AuthRejection,
) -> Result<Vec<u8>, Error> {
    *auth_rejection = AuthRejection::None;
    let mut body = Vec::new();
    let mut chunk = [0u8; TRANSFER_BUFFER_LEN];
    loop {
        let read_len = reader.read_slice_short(&mut chunk)?;
        if read_len > max_response_bytes.saturating_sub(body.len()) {
            return Err(Error::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk[..read_len]);
        if read_len < chunk.len() {
            break;
        }
    }
    match validate_final_response(&body, request_id, allow_discovery_error_id_mismatch) {
        Ok(()) => Ok(body),
        Err(err) => {
            if allow_discovery_error_id_mismatch {
                let rejection = discovery_authentication_rejection(&body)?;
                if rejection != AuthRejection::None {
                    *auth_rejection = rejection;
                    return Ok(body);
                }
            }
            Err(err)
        }
    }
}

fn discovery_authentication_rejection(body: &[u8]) -> Result<AuthRejection, Error> {
    let text = match std::str::from_utf8(body) {
        Ok(text) => text,
        Err(_) => return Ok(AuthRejection::None),
    };
    let parsed = match json_value::parse_json(text) {
        Ok(value) => value,
        Err(_) => return Ok(AuthRejection::None),
    };
    let object = match parsed.as_object() {
        Some(object) if !object.contains("jsonrpc") => object,
        _ => return Ok(AuthRejection::None),
    };
    let status = match object.get("error") {
        Some(value) => value,
        None => return Ok(AuthRejection::None),
    };
    let reason = match object.get("reason") {
        Some(value) => value,
        None => return Ok(AuthRejection::None),
    };
    let status = match json_integer(status) {
        Some(value) => value,
        None => return Ok(AuthRejection::None),
    };
    let reason = match reason.as_str() {
        Some(value) => value,
        None => return Ok(AuthRejection::None),
    };
    if status == 401 && reason == "Unauthorized" {
        return Ok(AuthRejection::Unauthorized);
    }
    if status == 403 && reason == "Forbidden" {
        return Ok(AuthRejection::InsufficientScope);
    }
    Ok(AuthRejection::None)
}

struct SseEvent {
    data: Vec<u8>,
}

struct SseParser {
    line: Vec<u8>,
    data: Vec<u8>,
    total_bytes: usize,
    event_count: usize,
    max_total_bytes: usize,
    max_event_bytes: usize,
    max_events: usize,
    pending_cr: bool,
}

impl SseParser {
    fn init(max_total_bytes: usize, max_event_bytes: usize, max_events: usize) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            total_bytes: 0,
            event_count: 0,
            max_total_bytes,
            max_event_bytes,
            max_events,
            pending_cr: false,
        }
    }

    fn feed(&mut self, chunk: &[u8], events: &mut Vec<SseEvent>) -> Result<(), Error> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or(Error::ResponseTooLarge)?;
        if self.total_bytes > self.max_total_bytes {
            return Err(Error::ResponseTooLarge);
        }

        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' {
                let line = std::mem::take(&mut self.line);
                self.process_line(&line, events)?;
                self.pending_cr = true;
                continue;
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.process_line(&line, events)?;
                continue;
            }
            if self.line.len() >= self.max_event_bytes {
                return Err(Error::SseEventTooLarge);
            }
            self.line.push(byte);
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), Error> {
        if !self.line.is_empty() || !self.data.is_empty() {
            Err(Error::BrokenSseStream)
        } else {
            Ok(())
        }
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) -> Result<(), Error> {
        if line.is_empty() {
            if self.data.is_empty() {
                return Ok(());
            }
            self.event_count = self
                .event_count
                .checked_add(1)
                .ok_or(Error::TooManySseEvents)?;
            if self.event_count > self.max_events {
                return Err(Error::TooManySseEvents);
            }
            let owned = std::mem::take(&mut self.data);
            events.push(SseEvent { data: owned });
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }

        let colon = line.iter().position(|&b| b == b':').unwrap_or(line.len());
        if &line[..colon] != b"data" {
            return Ok(());
        }
        let mut value = if colon < line.len() {
            &line[colon + 1..]
        } else {
            &[]
        };
        if !value.is_empty() && value[0] == b' ' {
            value = &value[1..];
        }
        let separator_len: usize = if self.data.is_empty() { 0 } else { 1 };
        let next_len = self
            .data
            .len()
            .checked_add(separator_len + value.len())
            .ok_or(Error::SseEventTooLarge)?;
        if next_len > self.max_event_bytes {
            return Err(Error::SseEventTooLarge);
        }
        if separator_len == 1 {
            self.data.push(b'\n');
        }
        self.data.extend_from_slice(value);
        Ok(())
    }
}

enum SseOutcome {
    Notification,
    Progress,
    Final,
}

fn classify_sse_event(
    data: &[u8],
    request_id: i64,
    progress_sink: Option<&mut ProgressSink<'_>>,
    notification_sink: Option<&mut NotificationSink<'_>>,
) -> Result<SseOutcome, Error> {
    let text = std::str::from_utf8(data).map_err(|_| Error::InvalidSseEvent)?;
    let parsed = json_value::parse_json(text).map_err(|_| Error::InvalidSseEvent)?;
    let object = parsed.as_object().ok_or(Error::InvalidSseEvent)?;
    let jsonrpc = object.get("jsonrpc").ok_or(Error::InvalidSseEvent)?;
    match jsonrpc.as_str() {
        Some("2.0") => {}
        _ => return Err(Error::InvalidSseEvent),
    }

    if let Some(id_value) = object.get("id") {
        let id = json_integer(id_value).ok_or(Error::MismatchedResponseId)?;
        if id != request_id {
            return Err(Error::MismatchedResponseId);
        }
        if object.contains("result") || object.contains("error") {
            return Ok(SseOutcome::Final);
        }
        return Err(Error::UnsupportedServerRequest);
    }

    let method_value = object.get("method").ok_or(Error::InvalidSseEvent)?;
    let method = method_value.as_str().ok_or(Error::InvalidSseEvent)?;
    if method != "notifications/progress" {
        if let Some(sink) = notification_sink {
            (sink.callback)(&parsed);
        }
        return Ok(SseOutcome::Notification);
    }
    let params_value = match object.get("params") {
        Some(value) => value,
        None => return Ok(SseOutcome::Notification),
    };
    let params = match params_value.as_object() {
        Some(object) => object,
        None => return Ok(SseOutcome::Notification),
    };
    let token_value = match params.get("progressToken") {
        Some(value) => value,
        None => return Ok(SseOutcome::Notification),
    };
    match json_integer(token_value) {
        Some(token) if token == request_id => {}
        _ => return Ok(SseOutcome::Notification),
    }
    let progress_value = match params.get("progress") {
        Some(value) => value,
        None => return Ok(SseOutcome::Notification),
    };
    let progress = match json_number_as_f64(progress_value) {
        Some(value) => value,
        None => return Ok(SseOutcome::Notification),
    };
    let total = params.get("total").and_then(json_number_as_f64);
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(sink) = progress_sink {
        (sink.callback)(Progress {
            progress,
            total,
            message,
        });
    }
    Ok(SseOutcome::Progress)
}

fn read_sse_response(
    reader: &mut dyn IoRead,
    request_id: i64,
    max_response_bytes: usize,
    max_event_bytes: usize,
    max_events: usize,
    mut progress_sink: Option<&mut ProgressSink<'_>>,
    mut notification_sink: Option<&mut NotificationSink<'_>>,
    cancellation: CancellationSources<'_>,
) -> Result<Vec<u8>, Error> {
    let mut parser = SseParser::init(max_response_bytes, max_event_bytes, max_events);
    let mut events = Vec::new();
    loop {
        if reader.buffered().is_empty() {
            match reader.fill_more() {
                Ok(()) => {}
                Err(Error::EndOfStream) => {
                    parser.finish()?;
                    return Err(Error::MissingFinalResponse);
                }
                Err(Error::ReadFailed) => return Err(Error::ReadFailed),
                Err(err) => return Err(err),
            }
        }
        let chunk_len = {
            let chunk = reader.buffered();
            if chunk.is_empty() {
                continue;
            }
            if let Some(index) = chunk.iter().position(|&b| b == b'\r' || b == b'\n') {
                index + 1
            } else {
                chunk.len()
            }
        };
        let chunk = reader.buffered()[..chunk_len].to_vec();
        parser.feed(&chunk, &mut events)?;
        reader.toss(chunk_len);
        for event in &events {
            match classify_sse_event(
                &event.data,
                request_id,
                progress_sink.as_deref_mut(),
                notification_sink.as_deref_mut(),
            )? {
                SseOutcome::Notification | SseOutcome::Progress => {}
                SseOutcome::Final => {
                    validate_final_response(&event.data, request_id, false)?;
                    return Ok(event.data.clone());
                }
            }
            if cancellation.cancelled() {
                return Err(Error::Cancelled);
            }
        }
        events.clear();
    }
}

fn validate_final_response(
    body: &[u8],
    request_id: i64,
    allow_discovery_error_id_mismatch: bool,
) -> Result<(), Error> {
    let text = std::str::from_utf8(body).map_err(|_| Error::InvalidJsonResponse)?;
    let parsed =
        json_value::parse_json(text).map_err(|_| Error::InvalidJsonResponse)?;
    let object = parsed.as_object().ok_or(Error::InvalidJsonResponse)?;
    let jsonrpc = object.get("jsonrpc").ok_or(Error::InvalidJsonResponse)?;
    match jsonrpc.as_str() {
        Some("2.0") => {}
        _ => return Err(Error::InvalidJsonResponse),
    }
    let id_value = match object.get("id") {
        Some(value) => value,
        None => {
            if allow_discovery_error_id_mismatch && is_legacy_discovery_compatibility_error(object)
            {
                return Ok(());
            }
            return Err(Error::InvalidJsonResponse);
        }
    };
    let matching_id = json_integer(id_value) == Some(request_id);
    let classifiable_discovery_error = allow_discovery_error_id_mismatch
        && object.contains("error")
        && !object.contains("result");
    if !matching_id && !classifiable_discovery_error {
        return Err(Error::MismatchedResponseId);
    }
    if !object.contains("result") && !object.contains("error") {
        return Err(Error::InvalidJsonResponse);
    }
    Ok(())
}

fn is_legacy_discovery_compatibility_error(object: &JsonMap) -> bool {
    if object.contains("result") {
        return false;
    }
    let error_value = match object.get("error") {
        Some(value) => value,
        None => return false,
    };
    let error_object = match error_value.as_object() {
        Some(object) => object,
        None => return false,
    };
    let code = match error_object.get("code").and_then(json_integer) {
        Some(code) => code,
        None => return false,
    };
    let message = match error_object.get("message").and_then(Value::as_str) {
        Some(message) => message,
        None => return false,
    };
    (code == LEGACY_SESSIONLESS_INVALID_REQUEST_CODE && message == "invalid request")
        || (code == LEGACY_SESSION_REQUIRED_CODE
            && message == "Bad Request: Mcp-Session-Id header is required")
        || (code == LEGACY_MISSING_SESSION_HEADER_CODE
            && message
                == "Missing required Mcp-Session-Id header. Non-initialize requests must include a session ID.")
}

fn is_legacy_discovery_compatibility_response(body: &[u8]) -> Result<bool, Error> {
    let text = match std::str::from_utf8(body) {
        Ok(text) => text,
        Err(_) => return Ok(false),
    };
    let parsed = match json_value::parse_json(text) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let object = match parsed.as_object() {
        Some(object) => object,
        None => return Ok(false),
    };
    Ok(is_legacy_discovery_compatibility_error(object))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_header<'a>(headers: &'a OwnedHeaders, name: &str) -> Option<&'a str> {
        headers
            .items
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    fn expect_header(headers: &OwnedHeaders, name: &str, value: &str) {
        assert_eq!(
            value,
            find_header(headers, name).expect("TestExpectedEqual")
        );
    }

    #[test]
    fn modern_mcp_endpoint_policy_accepts_https_and_explicit_loopback_http() {
        validate_endpoint("https://mcp.example.com/rpc?workspace=one").unwrap();
        validate_endpoint("http://localhost:4321/mcp").unwrap();
        validate_endpoint("http://127.0.0.1:4321/mcp").unwrap();
        validate_endpoint("http://[::1]:4321/mcp").unwrap();

        assert_eq!(
            Err(EndpointError::InsecureEndpoint),
            validate_endpoint("http://example.com/mcp")
        );
        assert_eq!(
            Err(EndpointError::InsecureEndpoint),
            validate_endpoint("http://localhost/mcp")
        );
        assert_eq!(
            Err(EndpointError::InvalidEndpoint),
            validate_endpoint("https://user@example.com/mcp")
        );
        assert_eq!(
            Err(EndpointError::InvalidEndpoint),
            validate_endpoint("https://example.com/mcp#fragment")
        );
    }

    #[test]
    fn modern_mcp_static_headers_reject_malformed_duplicate_and_protocol_owned_names() {
        let valid = [
            McpHttpHeader {
                name: "Authorization".into(),
                value: "Bearer redacted".into(),
            },
            McpHttpHeader {
                name: "X-Workspace".into(),
                value: "one".into(),
            },
        ];
        validate_static_headers(&valid).unwrap();

        let duplicate = [
            McpHttpHeader {
                name: "X-Workspace".into(),
                value: "one".into(),
            },
            McpHttpHeader {
                name: "x-workspace".into(),
                value: "two".into(),
            },
        ];
        assert_eq!(
            Err(HeaderError::DuplicateHeader),
            validate_static_headers(&duplicate)
        );

        let invalid_name = [McpHttpHeader {
            name: "Bad Header".into(),
            value: "value".into(),
        }];
        assert_eq!(
            Err(HeaderError::InvalidHeaderName),
            validate_static_headers(&invalid_name)
        );

        let injected = [McpHttpHeader {
            name: "X-Test".into(),
            value: "one\r\ntwo".into(),
        }];
        assert_eq!(
            Err(HeaderError::InvalidHeaderValue),
            validate_static_headers(&injected)
        );

        let reserved = [McpHttpHeader {
            name: "Mcp-Param-Region".into(),
            value: "override".into(),
        }];
        assert_eq!(
            Err(HeaderError::ReservedHeader),
            validate_static_headers(&reserved)
        );

        let legacy_reserved = [McpHttpHeader {
            name: "MCP-Session-Id".into(),
            value: "override".into(),
        }];
        assert_eq!(
            Err(HeaderError::ReservedHeader),
            validate_static_headers(&legacy_reserved)
        );
    }

    #[test]
    fn modern_mcp_request_headers_include_protocol_metadata_and_direct_scalar_projections() {
        let static_headers = [McpHttpHeader {
            name: "X-Workspace".into(),
            value: "one".into(),
        }];
        let request_body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}},"name":"echo","arguments":{"region":"us-west1","priority":42,"verbose":false,"unsafe":"Hello, 世界","padding":" padded ","empty":"","ignored":3.14,"nullable":null}}}"#;
        let schema_json = r#"{"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"},"priority":{"type":"integer","x-mcp-header":"Priority"},"verbose":{"type":"boolean","x-mcp-header":"Verbose"},"unsafe":{"type":"string","x-mcp-header":"Unsafe"},"padding":{"type":"string","x-mcp-header":"Padding"},"empty":{"type":"string","x-mcp-header":"Empty"},"ignored":{"type":"number"},"nullable":{"type":"string","x-mcp-header":"Nullable"}}}"#;

        let prepared = prepare_request(&static_headers, request_body, Some(schema_json)).unwrap();
        let headers = &prepared.headers;

        expect_header(headers, "Accept", "application/json, text/event-stream");
        expect_header(headers, "MCP-Protocol-Version", protocol_version);
        expect_header(headers, "Mcp-Method", "tools/call");
        expect_header(headers, "Mcp-Name", "echo");
        expect_header(headers, "X-Workspace", "one");
        expect_header(headers, "Mcp-Param-Region", "us-west1");
        expect_header(headers, "Mcp-Param-Priority", "42");
        expect_header(headers, "Mcp-Param-Verbose", "false");
        expect_header(headers, "Mcp-Param-Unsafe", "=?base64?SGVsbG8sIOS4lueVjA==?=");
        expect_header(headers, "Mcp-Param-Padding", "=?base64?IHBhZGRlZCA=?=");
        expect_header(headers, "Mcp-Param-Empty", "");
        assert!(find_header(headers, "Mcp-Param-Ignored").is_none());
        assert!(find_header(headers, "Mcp-Param-Nullable").is_none());
    }

    #[test]
    fn modern_mcp_request_headers_include_feature_identities() {
        let meta = r#"{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}}"#;
        let cases = [
            (
                format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{{"_meta":{meta},"uri":"Hello, 世界"}}}}"#
                ),
                "resources/read",
                "=?base64?SGVsbG8sIOS4lueVjA==?=",
            ),
            (
                format!(
                    r#"{{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{{"_meta":{meta},"name":"review","arguments":{{}}}}}}"#
                ),
                "prompts/get",
                "review",
            ),
        ];
        for (body, method, name) in cases {
            let prepared = prepare_request(&[], &body, None).unwrap();
            expect_header(&prepared.headers, "Mcp-Method", method);
            expect_header(&prepared.headers, "Mcp-Name", name);
        }

        let list_body = r#"{"jsonrpc":"2.0","id":3,"method":"resources/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}}}}"#;
        let list = prepare_request(&[], list_body, None).unwrap();
        assert!(find_header(&list.headers, "Mcp-Name").is_none());

        let invalid_bodies = [
            r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}}}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"prompts/get","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}},"name":7}}"#,
        ];
        for body in invalid_bodies {
            assert_eq!(
                Some(Error::InvalidModernRequest),
                prepare_request(&[], body, None).err()
            );
        }
    }

    #[test]
    fn http_discovery_carries_required_protocol_metadata() {
        let request_body = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}}}}"#;
        let initial = prepare_request(&[], request_body, None).unwrap();
        expect_header(&initial.headers, "MCP-Protocol-Version", protocol_version);
        expect_header(&initial.headers, "Mcp-Method", "server/discover");
    }

    #[test]
    fn modern_mcp_request_headers_project_nested_properties() {
        let request_body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}},"name":"echo","arguments":{"routing":{"region":"us-east1","options":{"verbose":true}}}}}"#;
        let schema_json = r#"{"type":"object","properties":{"routing":{"type":"object","allOf":[],"if":{"type":"object"},"properties":{"region":{"type":"string","x-mcp-header":"Region"},"options":{"type":"object","properties":{"verbose":{"type":"boolean","x-mcp-header":"Verbose"}}}}}}}"#;

        let prepared = prepare_request(&[], request_body, Some(schema_json)).unwrap();
        expect_header(&prepared.headers, "Mcp-Param-Region", "us-east1");
        expect_header(&prepared.headers, "Mcp-Param-Verbose", "true");
    }

    #[test]
    fn modern_mcp_projected_integers_stay_within_the_json_safe_range() {
        let schema_json = r#"{"type":"object","properties":{"minimum":{"type":"integer","x-mcp-header":"Minimum"},"maximum":{"type":"integer","x-mcp-header":"Maximum"}}}"#;
        let boundary_request = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}},"name":"echo","arguments":{"minimum":-9007199254740991,"maximum":9007199254740991}}}"#;
        let prepared = prepare_request(&[], boundary_request, Some(schema_json)).unwrap();
        expect_header(&prepared.headers, "Mcp-Param-Minimum", "-9007199254740991");
        expect_header(&prepared.headers, "Mcp-Param-Maximum", "9007199254740991");

        let outside_requests = [
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}},"name":"echo","arguments":{"minimum":-9007199254740992}}}"#,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}},"name":"echo","arguments":{"maximum":9007199254740992}}}"#,
        ];
        for request_body in outside_requests {
            assert_eq!(
                Some(Error::InvalidProjectedHeaderValue),
                prepare_request(&[], request_body, Some(schema_json)).err()
            );
        }
    }

    #[test]
    fn modern_mcp_request_metadata_and_projected_values_are_validated_before_network_i_o() {
        let missing_meta = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        assert_eq!(
            Some(Error::InvalidModernRequest),
            prepare_request(&[], missing_meta, None).err()
        );

        let wrong_version = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2025-06-18","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
        assert_eq!(
            Some(Error::InvalidModernRequest),
            prepare_request(&[], wrong_version, None).err()
        );

        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"fx","version":"test"},"io.modelcontextprotocol/clientCapabilities":{}},"name":"echo","arguments":{"region":12}}}"#;
        assert_eq!(
            Some(Error::MissingToolInputSchema),
            prepare_request(&[], call, None).err()
        );
        assert_eq!(
            Some(Error::InvalidProjectedHeaderValue),
            prepare_request(
                &[],
                call,
                Some(r#"{"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}}"#),
            )
            .err()
        );
    }

    #[test]
    fn modern_mcp_tool_header_annotations_reject_ambiguous_schemas_without_dereferencing() {
        validate_tool_input_schema_headers(
            r#"{"type":"object","properties":{"local":{"$ref":"https://example.invalid/schema.json"}}}"#,
        )
        .unwrap();

        let invalid_schemas: &[(&str, Error)] = &[
            (
                r#"{"type":"object","x-mcp-header":"Root","properties":{}}"#,
                Error::InvalidHeaderAnnotationPlacement,
            ),
            (
                r#"{"type":"object","properties":{"value":{"type":"string","x-mcp-header":""}}}"#,
                Error::InvalidHeaderAnnotationName,
            ),
            (
                r#"{"type":"object","properties":{"value":{"type":"number","x-mcp-header":"Value"}}}"#,
                Error::InvalidHeaderAnnotationType,
            ),
            (
                r#"{"type":"object","properties":{"one":{"type":"string","x-mcp-header":"Region"},"two":{"type":"string","x-mcp-header":"region"}}}"#,
                Error::DuplicateHeaderAnnotation,
            ),
            (
                r##"{"type":"object","properties":{"value":{"type":"string","x-mcp-header":"Value","$ref":"#/$defs/value"}}}"##,
                Error::InvalidHeaderAnnotationType,
            ),
            (
                r#"{"type":"object","properties":{"values":{"type":"array","items":{"type":"string","x-mcp-header":"Item"}}}}"#,
                Error::InvalidHeaderAnnotationPlacement,
            ),
            (
                r#"{"type":"object","properties":{"value":{"oneOf":[{"type":"string","x-mcp-header":"Choice"}]}}}"#,
                Error::InvalidHeaderAnnotationPlacement,
            ),
            (
                r#"{"type":"object","properties":{"value":{"if":{"type":"string","x-mcp-header":"Conditional"}}}}"#,
                Error::InvalidHeaderAnnotationPlacement,
            ),
        ];
        for &(schema, expected) in invalid_schemas {
            assert_eq!(Err(expected), validate_tool_input_schema_headers(schema));
        }
    }

    #[test]
    fn modern_mcp_tool_header_annotation_names_are_unique_across_nested_properties() {
        assert_eq!(
            Err(Error::DuplicateHeaderAnnotation),
            validate_tool_input_schema_headers(
                r#"{"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"},"nested":{"type":"object","properties":{"value":{"type":"string","x-mcp-header":"region"}}}}}"#,
            )
        );
    }

    #[test]
    fn modern_mcp_content_length_reader_leaves_bytes_after_the_declared_body_unread() {
        let mut source = SliceReader::fixed(b"bodyafter");
        let mut reader_buffer = [0u8; 3];
        let mut bounded = McpContentLengthReader::init(&mut source, 4, &mut reader_buffer);

        let mut body = [0u8; 8];
        let body_len = bounded.interface.read_slice_short(&mut body).unwrap();
        assert_eq!(4, body_len);
        assert_eq!(b"body", &body[..body_len]);
        assert_eq!(
            Err(Error::EndOfStream),
            bounded.interface.read_slice_all(&mut body[..1])
        );

        let mut after = [0u8; 5];
        source.read_slice_all(&mut after).unwrap();
        assert_eq!(b"after", &after);
    }

    #[test]
    fn modern_mcp_content_length_reader_applies_one_bound_across_discard_and_read() {
        let mut source = SliceReader::fixed(b"abcdef");
        let mut reader_buffer = [0u8; 3];
        let mut bounded = McpContentLengthReader::init(&mut source, 4, &mut reader_buffer);

        bounded.interface.discard_all(2).unwrap();
        let mut body = [0u8; 2];
        bounded.interface.read_slice_all(&mut body).unwrap();
        assert_eq!(b"cd", &body);
        assert_eq!(
            Err(Error::EndOfStream),
            bounded.interface.discard_all(1)
        );

        let mut after = [0u8; 2];
        source.read_slice_all(&mut after).unwrap();
        assert_eq!(b"ef", &after);
    }

    #[test]
    fn modern_mcp_zero_content_length_does_not_consume_the_source() {
        let mut source = SliceReader::fixed(b"x");
        let mut reader_buffer = [0u8; 1];
        let mut bounded = McpContentLengthReader::init(&mut source, 0, &mut reader_buffer);

        let mut byte = [0u8; 1];
        assert_eq!(
            Err(Error::EndOfStream),
            bounded.interface.read_slice_all(&mut byte)
        );
        source.read_slice_all(&mut byte).unwrap();
        assert_eq!(b"x", &byte);
    }

    #[test]
    fn modern_mcp_content_length_reader_supports_incremental_buffered_reads() {
        let mut source = SliceReader::fixed(b"abcdef");
        let mut reader_buffer = [0u8; 2];
        let mut bounded = McpContentLengthReader::init(&mut source, 4, &mut reader_buffer);

        bounded.interface.fill_more().unwrap();
        assert_eq!(b"ab", bounded.interface.buffered());
        bounded.interface.toss(2);
        bounded.interface.fill_more().unwrap();
        assert_eq!(b"cd", bounded.interface.buffered());
        bounded.interface.toss(2);
        assert_eq!(
            Err(Error::EndOfStream),
            bounded.interface.fill_more()
        );

        let mut after = [0u8; 2];
        source.read_slice_all(&mut after).unwrap();
        assert_eq!(b"ef", &after);
    }

    #[test]
    fn modern_mcp_response_framing_isolates_only_known_content_length_bodies() {
        assert_eq!(
            McpResponseBodyFraming::ContentLength,
            mcp_response_body_framing(TransferEncoding::None, Some(42))
        );
        assert_eq!(
            McpResponseBodyFraming::Standard,
            mcp_response_body_framing(TransferEncoding::None, None)
        );
        assert_eq!(
            McpResponseBodyFraming::Standard,
            mcp_response_body_framing(TransferEncoding::Chunked, Some(42))
        );
    }

    #[test]
    fn modern_mcp_json_responses_are_bounded_and_retain_request_ownership() {
        let payload = r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;

        let mut reader = SliceReader::fixed(payload.as_bytes());
        let body = read_json_response(&mut reader, 7, payload.len(), false).unwrap();
        assert_eq!(payload.as_bytes(), body.as_slice());

        let mut oversized_reader = SliceReader::fixed(payload.as_bytes());
        assert_eq!(
            Err(Error::ResponseTooLarge),
            read_json_response(&mut oversized_reader, 7, payload.len() - 1, false)
        );

        let mut wrong_id_reader = SliceReader::fixed(br#"{"jsonrpc":"2.0","id":8,"result":{}}"#);
        assert_eq!(
            Err(Error::MismatchedResponseId),
            read_json_response(&mut wrong_id_reader, 7, 128, false)
        );

        let sdk_error =
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Server not initialized"}}"#;
        let mut sdk_error_reader = SliceReader::fixed(sdk_error.as_bytes());
        let sdk_error_body =
            read_json_response(&mut sdk_error_reader, 7, sdk_error.len(), true).unwrap();
        assert_eq!(sdk_error.as_bytes(), sdk_error_body.as_slice());

        let mut strict_sdk_error_reader = SliceReader::fixed(sdk_error.as_bytes());
        assert_eq!(
            Err(Error::MismatchedResponseId),
            read_json_response(&mut strict_sdk_error_reader, 7, sdk_error.len(), false)
        );

        let string_id_discovery_error = r#"{"jsonrpc":"2.0","id":"server-error","error":{"code":-32600,"message":"Unsupported protocol version"}}"#;
        let mut string_id_discovery_error_reader =
            SliceReader::fixed(string_id_discovery_error.as_bytes());
        let string_id_discovery_error_body = read_json_response(
            &mut string_id_discovery_error_reader,
            7,
            string_id_discovery_error.len(),
            true,
        )
        .unwrap();
        assert_eq!(
            string_id_discovery_error.as_bytes(),
            string_id_discovery_error_body.as_slice()
        );

        let mongodb_session_error =
            r#"{"jsonrpc":"2.0","error":{"code":-32004,"message":"invalid request"}}"#;
        let mut mongodb_session_error_reader =
            SliceReader::fixed(mongodb_session_error.as_bytes());
        let mongodb_session_error_body = read_json_response(
            &mut mongodb_session_error_reader,
            7,
            mongodb_session_error.len(),
            true,
        )
        .unwrap();
        assert_eq!(
            mongodb_session_error.as_bytes(),
            mongodb_session_error_body.as_slice()
        );

        let gitmcp_session_error = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Bad Request: Mcp-Session-Id header is required"}}"#;
        let gitmcp_parsed = json_value::parse_json(gitmcp_session_error).unwrap();
        assert!(is_legacy_discovery_compatibility_error(
            gitmcp_parsed.as_object().unwrap()
        ));

        let stock_session_error = r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"Bad Request: Mcp-Session-Id header is required"}}"#;
        let mut stock_session_error_reader = SliceReader::fixed(stock_session_error.as_bytes());
        let stock_session_error_body = read_json_response(
            &mut stock_session_error_reader,
            7,
            stock_session_error.len(),
            true,
        )
        .unwrap();
        assert_eq!(
            stock_session_error.as_bytes(),
            stock_session_error_body.as_slice()
        );

        let mongodb_deployed_session_error = r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"Missing required Mcp-Session-Id header. Non-initialize requests must include a session ID."}}"#;
        let mut mongodb_deployed_session_error_reader =
            SliceReader::fixed(mongodb_deployed_session_error.as_bytes());
        let mongodb_deployed_session_error_body = read_json_response(
            &mut mongodb_deployed_session_error_reader,
            7,
            mongodb_deployed_session_error.len(),
            true,
        )
        .unwrap();
        assert_eq!(
            mongodb_deployed_session_error.as_bytes(),
            mongodb_deployed_session_error_body.as_slice()
        );

        let unrelated_session_error = json_value::parse_json(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Session expired"}}"#,
        )
        .unwrap();
        assert!(!is_legacy_discovery_compatibility_error(
            unrelated_session_error.as_object().unwrap()
        ));

        let unrelated_missing_id_error =
            r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"}}"#;
        let mut unrelated_missing_id_reader =
            SliceReader::fixed(unrelated_missing_id_error.as_bytes());
        assert_eq!(
            Err(Error::InvalidJsonResponse),
            read_json_response(
                &mut unrelated_missing_id_reader,
                7,
                unrelated_missing_id_error.len(),
                true,
            )
        );

        let string_id_success = r#"{"jsonrpc":"2.0","id":"server-error","result":{}}"#;
        let mut string_id_success_reader = SliceReader::fixed(string_id_success.as_bytes());
        assert_eq!(
            Err(Error::MismatchedResponseId),
            read_json_response(
                &mut string_id_success_reader,
                7,
                string_id_success.len(),
                true,
            )
        );
    }

    #[test]
    fn mcp_discovery_recognizes_bounded_authorization_error_documents() {
        let unauthorized = r#"{"error":401,"reason":"Unauthorized","detail":"You are not authorized for this resource."}"#;
        assert_eq!(
            AuthRejection::Unauthorized,
            discovery_authentication_rejection(unauthorized.as_bytes()).unwrap()
        );
        let mut strict_reader = SliceReader::fixed(unauthorized.as_bytes());
        assert_eq!(
            Err(Error::InvalidJsonResponse),
            read_json_response(&mut strict_reader, 7, unauthorized.len(), true)
        );
        assert_eq!(
            AuthRejection::InsufficientScope,
            discovery_authentication_rejection(
                br#"{"error":403,"reason":"Forbidden","detail":"Insufficient scope."}"#
            )
            .unwrap()
        );
        assert_eq!(
            AuthRejection::None,
            discovery_authentication_rejection(
                br#"{"error":401,"reason":"not an authorization response"}"#
            )
            .unwrap()
        );
        assert_eq!(
            AuthRejection::None,
            discovery_authentication_rejection(b"not json").unwrap()
        );
    }

    fn expect_modern_sse_delimiter_splits(payload: &[u8]) {
        for split in 0..=payload.len() {
            let mut parser = SseParser::init(payload.len(), 64, 1);
            let mut events = Vec::new();
            parser.feed(&payload[..split], &mut events).unwrap();
            parser.feed(&payload[split..], &mut events).unwrap();
            parser.finish().unwrap();
            assert_eq!(1, events.len());
            assert_eq!(b"one\ntwo", events[0].data.as_slice());
        }
    }

    #[test]
    fn modern_mcp_sse_accepts_cr_lf_and_crlf_across_every_chunk_split() {
        for payload in [
            b"data: one\ndata: two\n\n".as_slice(),
            b"data: one\rdata: two\r\r".as_slice(),
            b"data: one\r\ndata: two\r\n\r\n".as_slice(),
            b"data: one\r\ndata: two\r\r\n".as_slice(),
        ] {
            expect_modern_sse_delimiter_splits(payload);
        }
    }

    #[test]
    fn modern_mcp_sse_incrementally_accepts_notifications_progress_and_one_final_response() {
        let payload = concat!(
            ": keepalive\n",
            "event: message\n",
            r#"data: {"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}"#,
            "\n",
            "\n",
            "id: ignored\n",
            r#"data: {"jsonrpc":"2.0","method":"notifications/progress","#,
            "\n",
            r#"data: "params":{"progressToken":7,"progress":1,"total":2,"message":"half"}}"#,
            "\n",
            "\n",
            r#"data: {"jsonrpc":"2.0","id":7,"result":{"content":[]}}"#,
            "\n",
            "\n",
            "\n",
        );
        let mut calls = 0usize;
        let mut progress_val = 0f64;
        let mut total_val: Option<f64> = None;
        let mut message_seen = false;
        let mut progress_cb = |progress: Progress| {
            calls += 1;
            progress_val = progress.progress;
            total_val = progress.total;
            message_seen = progress.message.as_deref() == Some("half");
        };
        let mut progress_sink = ProgressSink {
            callback: &mut progress_cb,
        };
        let mut reader = SliceReader::fixed(payload.as_bytes());
        let body = read_sse_response(
            &mut reader,
            7,
            payload.len(),
            512,
            3,
            Some(&mut progress_sink),
            None,
            CancellationSources {
                caller: None,
                runtime: None,
            },
        )
        .unwrap();

        assert_eq!(1, calls);
        assert_eq!(1.0, progress_val);
        assert_eq!(Some(2.0), total_val);
        assert!(message_seen);
        assert_eq!(
            br#"{"jsonrpc":"2.0","id":7,"result":{"content":[]}}"#,
            body.as_slice()
        );
    }

    #[test]
    fn modern_mcp_sse_stops_when_a_notification_cancels_the_operation() {
        let cancelled = AtomicBool::new(false);
        let mut calls = 0usize;
        let mut notification_cb = |_value: &Value| {
            calls += 1;
            cancelled.store(true, Ordering::Release);
        };
        let mut notification_sink = NotificationSink {
            callback: &mut notification_cb,
        };
        let payload =
            b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/subscriptions/acknowledged\",\"params\":{}}\n\n";
        let mut reader = SliceReader::fixed(payload);

        assert_eq!(
            Err(Error::Cancelled),
            read_sse_response(
                &mut reader,
                7,
                512,
                256,
                2,
                None,
                Some(&mut notification_sink),
                CancellationSources {
                    caller: Some(&cancelled),
                    runtime: None,
                },
            )
        );
        assert_eq!(1, calls);
    }

    #[test]
    fn modern_mcp_sse_rejects_broken_ownership_and_exceeded_bounds() {
        let mut broken_reader =
            SliceReader::fixed(br#"data: {"jsonrpc":"2.0","id":7,"result":{}}"#);
        assert_eq!(
            Err(Error::BrokenSseStream),
            read_sse_response(
                &mut broken_reader,
                7,
                256,
                128,
                2,
                None,
                None,
                CancellationSources {
                    caller: None,
                    runtime: None,
                },
            )
        );

        let mut wrong_id_reader =
            SliceReader::fixed(b"data: {\"jsonrpc\":\"2.0\",\"id\":8,\"result\":{}}\n\n");
        assert_eq!(
            Err(Error::MismatchedResponseId),
            read_sse_response(
                &mut wrong_id_reader,
                7,
                256,
                128,
                2,
                None,
                None,
                CancellationSources {
                    caller: None,
                    runtime: None,
                },
            )
        );

        let event_cap_payload = concat!(
            r#"data: {"jsonrpc":"2.0","method":"notifications/one"}"#,
            "\n\n",
            r#"data: {"jsonrpc":"2.0","id":7,"result":{}}"#,
            "\n\n",
        );
        let mut event_cap_reader = SliceReader::fixed(event_cap_payload.as_bytes());
        assert_eq!(
            Err(Error::TooManySseEvents),
            read_sse_response(
                &mut event_cap_reader,
                7,
                512,
                128,
                1,
                None,
                None,
                CancellationSources {
                    caller: None,
                    runtime: None,
                },
            )
        );

        let mut byte_cap_reader =
            SliceReader::fixed(b"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n");
        assert_eq!(
            Err(Error::ResponseTooLarge),
            read_sse_response(
                &mut byte_cap_reader,
                7,
                16,
                128,
                2,
                None,
                None,
                CancellationSources {
                    caller: None,
                    runtime: None,
                },
            )
        );
    }

    #[test]
    fn modern_mcp_sse_completes_at_the_matching_final_response() {
        let first = b"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        let mut payload = first.to_vec();
        payload.extend_from_slice(b"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n");
        let mut reader = SliceReader::fixed(&payload);
        let body = read_sse_response(
            &mut reader,
            7,
            first.len(),
            128,
            2,
            None,
            None,
            CancellationSources {
                caller: None,
                runtime: None,
            },
        )
        .unwrap();
        assert_eq!(br#"{"jsonrpc":"2.0","id":7,"result":{}}"#, body.as_slice());
    }

    #[test]
    fn modern_mcp_response_media_types_tolerate_case_and_parameters_only() {
        assert_eq!(
            Some(MediaType::Json),
            parse_media_type("Application/JSON; charset=utf-8")
        );
        assert_eq!(
            Some(MediaType::Sse),
            parse_media_type(" text/event-stream ; charset=utf-8")
        );
        assert!(parse_media_type("application/problem+json").is_none());
        assert!(parse_media_type("text/plain").is_none());
    }

    #[test]
    fn modern_mcp_cancellation_and_deadline_fail_before_network_admission() {
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            Err(Error::Cancelled),
            post(PostOptions {
                url: "http://127.0.0.1:1/mcp",
                static_headers: &[],
                request_body: "{}",
                input_schema_json: None,
                max_response_bytes: 128,
                max_event_bytes: 128,
                allow_version_error: false,
                allow_discovery_mismatch_status: false,
                progress: None,
                notifications: None,
                long_lived: false,
                precommit: None,
                control: Control {
                    deadline: io::milli_timestamp(),
                    cancel_flag: Some(&cancelled),
                    lifecycle_cancel_flag: None,
                },
            })
        );

        let caller_cancelled = AtomicBool::new(false);
        let runtime_retired = AtomicBool::new(true);
        assert_eq!(
            Err(Error::Cancelled),
            post(PostOptions {
                url: "http://127.0.0.1:1/mcp",
                static_headers: &[],
                request_body: "{}",
                input_schema_json: None,
                max_response_bytes: 128,
                max_event_bytes: 128,
                allow_version_error: false,
                allow_discovery_mismatch_status: false,
                progress: None,
                notifications: None,
                long_lived: false,
                precommit: None,
                control: Control {
                    deadline: io::milli_timestamp() + 1000,
                    cancel_flag: Some(&caller_cancelled),
                    lifecycle_cancel_flag: Some(&runtime_retired),
                },
            })
        );
        assert!(!caller_cancelled.load(Ordering::Acquire));

        let expired = io::milli_timestamp();
        assert_eq!(
            Err(Error::McpRequestTimedOut),
            post(PostOptions {
                url: "http://127.0.0.1:1/mcp",
                static_headers: &[],
                request_body: "{}",
                input_schema_json: None,
                max_response_bytes: 128,
                max_event_bytes: 128,
                allow_version_error: false,
                allow_discovery_mismatch_status: false,
                progress: None,
                notifications: None,
                long_lived: false,
                precommit: None,
                control: Control {
                    deadline: expired,
                    cancel_flag: None,
                    lifecycle_cancel_flag: None,
                },
            })
        );
    }
}
