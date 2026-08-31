//! Chunk-independent canonicalization of foreground command output.
//!
//! Ported from `zig-reference/src/core/tooling/command_output_content.zig`.

use crate::core::shared::types::ToolLifecycleId;
use std::ffi::c_void;

pub const MAX_FOREGROUND_RESULT_ENVELOPE_BYTES: usize = "exit_code=-9223372036854775808\n".len()
    + "<stdout>\n".len()
    + "\n</stdout>\n".len()
    + "<stderr>\n".len()
    + "\n</stderr>\n".len();
#[allow(non_upper_case_globals)]
pub const max_foreground_result_envelope_bytes: usize = MAX_FOREGROUND_RESULT_ENVELOPE_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stream {
    Stdout,
    Stderr,
}

pub type Callback<E> = fn(*mut c_void, Option<ToolLifecycleId>, Stream, &[u8]) -> Result<(), E>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransformKind {
    ByteEscape,
    C1Escape,
    EscapeByte,
    BareCr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransformEvent {
    pub visible_offset: usize,
    pub kind: TransformKind,
    pub byte: u8,
}

pub trait DecoderSink {
    type Error;
    fn append_text(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
    fn finish_line(&mut self) -> Result<(), Self::Error>;
    fn replace_line(&mut self) -> Result<(), Self::Error>;
    fn record_transform(&mut self, _kind: TransformKind, _byte: u8) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EscapeState {
    #[default]
    None,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Decoder {
    utf8_pending: [u8; 4],
    utf8_pending_len: usize,
    pending_cr: bool,
    escape_state: EscapeState,
    escape_bytes: [u8; 64],
    escape_len: usize,
    escape_overflow: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            utf8_pending: [0; 4],
            utf8_pending_len: 0,
            pending_cr: false,
            escape_state: EscapeState::None,
            escape_bytes: [0; 64],
            escape_len: 0,
            escape_overflow: false,
        }
    }
}

impl Decoder {
    pub fn append<S: DecoderSink>(&mut self, raw: &[u8], sink: &mut S) -> Result<(), S::Error> {
        for &byte in raw {
            self.consume_byte(byte, sink)?;
        }
        Ok(())
    }

    pub fn finish<S: DecoderSink>(&mut self, sink: &mut S) -> Result<(), S::Error> {
        self.pending_cr = false;
        self.flush_utf8(sink, true)?;
        if self.escape_state != EscapeState::None {
            self.flush_incomplete_escape(sink)?;
        }
        Ok(())
    }

    pub fn is_idle(&self) -> bool {
        self.utf8_pending_len == 0 && !self.pending_cr && self.escape_state == EscapeState::None
    }

    fn consume_byte<S: DecoderSink>(&mut self, byte: u8, sink: &mut S) -> Result<(), S::Error> {
        if self.escape_state != EscapeState::None && self.consume_escape_byte(byte, sink)? {
            return Ok(());
        }
        if self.pending_cr {
            self.pending_cr = false;
            if byte == b'\n' {
                self.flush_utf8(sink, true)?;
                sink.finish_line()?;
                return Ok(());
            }
            sink.replace_line()?;
            sink.record_transform(TransformKind::BareCr, b'\r')?;
        }
        match byte {
            0x1b => {
                self.flush_utf8(sink, true)?;
                sink.record_transform(TransformKind::EscapeByte, byte)?;
                self.start_escape();
            }
            b'\r' => {
                self.flush_utf8(sink, true)?;
                self.pending_cr = true;
            }
            b'\n' => {
                self.flush_utf8(sink, true)?;
                sink.finish_line()?;
            }
            0x80..=0xff => self.append_utf8_byte(byte, sink)?,
            b'\t' | 0x20..=0x7e => {
                self.flush_utf8(sink, true)?;
                sink.append_text(&[byte])?;
            }
            _ => {
                self.flush_utf8(sink, true)?;
                emit_escaped_byte(sink, TransformKind::ByteEscape, byte)?;
            }
        }
        Ok(())
    }

    fn append_utf8_byte<S: DecoderSink>(&mut self, byte: u8, sink: &mut S) -> Result<(), S::Error> {
        if self.utf8_pending_len == 4 {
            emit_escaped_byte(sink, TransformKind::ByteEscape, self.utf8_pending[0])?;
            self.shift_utf8_pending(1);
        }
        self.utf8_pending[self.utf8_pending_len] = byte;
        self.utf8_pending_len += 1;
        self.flush_utf8(sink, false)
    }

    fn flush_utf8<S: DecoderSink>(
        &mut self,
        sink: &mut S,
        final_chunk: bool,
    ) -> Result<(), S::Error> {
        while self.utf8_pending_len > 0 {
            let first = self.utf8_pending[0];
            let sequence_len = match first {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => 1,
            };
            if sequence_len == 1 {
                emit_escaped_byte(sink, TransformKind::ByteEscape, first)?;
                self.shift_utf8_pending(1);
                continue;
            }
            if self.utf8_pending_len < sequence_len {
                if !final_chunk {
                    return Ok(());
                }
                emit_escaped_byte(sink, TransformKind::ByteEscape, first)?;
                self.shift_utf8_pending(1);
                continue;
            }
            let sequence = &self.utf8_pending[..sequence_len];
            let Ok(text) = std::str::from_utf8(sequence) else {
                emit_escaped_byte(sink, TransformKind::ByteEscape, first)?;
                self.shift_utf8_pending(1);
                continue;
            };
            let scalar = text.chars().next().unwrap() as u32;
            if (0x80..=0x9f).contains(&scalar) {
                let sequence = sequence.to_vec();
                for &byte in &sequence {
                    sink.record_transform(TransformKind::C1Escape, byte)?;
                }
                let escaped = format!("\\u{{{scalar:04x}}}");
                sink.append_text(escaped.as_bytes())?;
            } else {
                let sequence = sequence.to_vec();
                sink.append_text(&sequence)?;
            }
            self.shift_utf8_pending(sequence_len);
        }
        Ok(())
    }

    fn shift_utf8_pending(&mut self, count: usize) {
        self.utf8_pending
            .copy_within(count..self.utf8_pending_len, 0);
        self.utf8_pending_len -= count;
    }
    fn start_escape(&mut self) {
        self.escape_state = EscapeState::Escape;
        self.escape_len = 0;
        self.escape_overflow = false;
        self.record_escape_byte(0x1b);
    }
    fn consume_escape_byte<S: DecoderSink>(
        &mut self,
        byte: u8,
        sink: &mut S,
    ) -> Result<bool, S::Error> {
        match self.escape_state {
            EscapeState::None => unreachable!(),
            EscapeState::Escape => match byte {
                b'[' => {
                    sink.record_transform(TransformKind::EscapeByte, byte)?;
                    self.record_escape_byte(byte);
                    self.escape_state = EscapeState::Csi;
                    Ok(true)
                }
                b']' => {
                    sink.record_transform(TransformKind::EscapeByte, byte)?;
                    self.record_escape_byte(byte);
                    self.escape_state = EscapeState::Osc;
                    Ok(true)
                }
                0x40..=0x5f => {
                    sink.record_transform(TransformKind::EscapeByte, byte)?;
                    self.reset_escape();
                    Ok(true)
                }
                _ => {
                    self.flush_incomplete_escape(sink)?;
                    Ok(false)
                }
            },
            EscapeState::Csi => {
                sink.record_transform(TransformKind::EscapeByte, byte)?;
                self.record_escape_byte(byte);
                if (0x40..=0x7e).contains(&byte) {
                    self.reset_escape();
                } else if byte < 0x20 || byte == 0x7f {
                    self.flush_incomplete_escape(sink)?;
                }
                Ok(true)
            }
            EscapeState::Osc => {
                sink.record_transform(TransformKind::EscapeByte, byte)?;
                self.record_escape_byte(byte);
                if byte == 0x07 {
                    self.reset_escape();
                } else if byte == 0x1b {
                    self.escape_state = EscapeState::OscEscape;
                }
                Ok(true)
            }
            EscapeState::OscEscape => {
                sink.record_transform(TransformKind::EscapeByte, byte)?;
                self.record_escape_byte(byte);
                if byte == b'\\' {
                    self.reset_escape();
                } else if byte != 0x1b {
                    self.escape_state = EscapeState::Osc;
                }
                Ok(true)
            }
        }
    }
    fn record_escape_byte(&mut self, byte: u8) {
        if self.escape_len < self.escape_bytes.len() {
            self.escape_bytes[self.escape_len] = byte;
            self.escape_len += 1;
        } else {
            self.escape_overflow = true;
        }
    }
    fn flush_incomplete_escape<S: DecoderSink>(&mut self, sink: &mut S) -> Result<(), S::Error> {
        let bytes = self.escape_bytes[..self.escape_len].to_vec();
        for byte in bytes {
            if byte < 0x20 || byte == 0x7f {
                emit_byte_escape(sink, byte)?;
            } else {
                sink.append_text(&[byte])?;
            }
        }
        if self.escape_overflow {
            sink.append_text(b"...")?;
        }
        self.reset_escape();
        Ok(())
    }
    fn reset_escape(&mut self) {
        self.escape_state = EscapeState::None;
        self.escape_len = 0;
        self.escape_overflow = false;
    }
}

fn emit_escaped_byte<S: DecoderSink>(
    sink: &mut S,
    kind: TransformKind,
    byte: u8,
) -> Result<(), S::Error> {
    sink.record_transform(kind, byte)?;
    emit_byte_escape(sink, byte)
}
fn emit_byte_escape<S: DecoderSink>(sink: &mut S, byte: u8) -> Result<(), S::Error> {
    let digits = b"0123456789abcdef";
    sink.append_text(&[
        b'\\',
        b'x',
        digits[(byte >> 4) as usize],
        digits[(byte & 15) as usize],
    ])
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalRecord {
    pub stream: Stream,
    pub text: Vec<u8>,
    pub transforms: Vec<TransformEvent>,
    pub terminated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalOutput {
    pub records: Vec<CanonicalRecord>,
    decoders: [Decoder; 2],
    open_record_indices: [Option<usize>; 2],
}

impl Default for CanonicalOutput {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            decoders: [Decoder::default(), Decoder::default()],
            open_record_indices: [None, None],
        }
    }
}

impl CanonicalOutput {
    pub fn append(&mut self, stream: Stream, raw: &[u8]) {
        let index = stream_index(stream);
        let mut decoder = std::mem::take(&mut self.decoders[index]);
        {
            let mut sink = CollectorSink {
                output: self,
                stream,
            };
            if !raw.is_empty() {
                sink.begin_record();
            }
            decoder.append(raw, &mut sink).unwrap();
        }
        self.decoders[index] = decoder;
    }
    pub fn finish(&mut self) {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let index = stream_index(stream);
            let mut decoder = std::mem::take(&mut self.decoders[index]);
            {
                let mut sink = CollectorSink {
                    output: self,
                    stream,
                };
                decoder.finish(&mut sink).unwrap();
            }
            self.decoders[index] = decoder;
            if let Some(record_index) = self.open_record_indices[index] {
                if self.records[record_index].text.is_empty()
                    && !self.records[record_index].terminated
                {
                    self.remove_record(record_index);
                }
            }
            self.open_record_indices[index] = None;
        }
    }
    fn remove_record(&mut self, record_index: usize) {
        self.records.remove(record_index);
        for maybe_index in &mut self.open_record_indices {
            if let Some(index) = *maybe_index {
                *maybe_index = if index == record_index {
                    None
                } else if index > record_index {
                    Some(index - 1)
                } else {
                    Some(index)
                };
            }
        }
    }
}

struct CollectorSink<'a> {
    output: &'a mut CanonicalOutput,
    stream: Stream,
}
impl CollectorSink<'_> {
    fn begin_record(&mut self) {
        self.ensure_open_record();
    }
    fn ensure_open_record(&mut self) -> usize {
        let stream_index = stream_index(self.stream);
        if let Some(index) = self.output.open_record_indices[stream_index] {
            return index;
        }
        self.output.records.push(CanonicalRecord {
            stream: self.stream,
            text: Vec::new(),
            transforms: Vec::new(),
            terminated: false,
        });
        let index = self.output.records.len() - 1;
        self.output.open_record_indices[stream_index] = Some(index);
        index
    }
}
impl DecoderSink for CollectorSink<'_> {
    type Error = std::convert::Infallible;
    fn append_text(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        let i = self.ensure_open_record();
        self.output.records[i].text.extend_from_slice(bytes);
        Ok(())
    }
    fn finish_line(&mut self) -> Result<(), Self::Error> {
        let stream = stream_index(self.stream);
        let i = self.ensure_open_record();
        self.output.records[i].terminated = true;
        self.output.open_record_indices[stream] = None;
        Ok(())
    }
    fn replace_line(&mut self) -> Result<(), Self::Error> {
        let i = self.ensure_open_record();
        self.output.records[i].text.clear();
        self.output.records[i].transforms.clear();
        self.output.records[i].terminated = false;
        Ok(())
    }
    fn record_transform(&mut self, kind: TransformKind, byte: u8) -> Result<(), Self::Error> {
        let i = self.ensure_open_record();
        let visible_offset = self.output.records[i].text.len();
        self.output.records[i].transforms.push(TransformEvent {
            visible_offset,
            kind,
            byte,
        });
        Ok(())
    }
}
fn stream_index(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        Stream::Stderr => 1,
    }
}

pub fn eql(lhs: &CanonicalOutput, rhs: &CanonicalOutput) -> bool {
    lhs.records.len() == rhs.records.len()
        && lhs.records.iter().zip(&rhs.records).all(|(left, right)| {
            left.stream == right.stream
                && left.text == right.text
                && left.transforms == right.transforms
        })
}

pub fn canonicalize_foreground_result(result: &[u8]) -> Option<CanonicalOutput> {
    let status_end = result.iter().position(|&b| b == b'\n')?;
    if !valid_foreground_status(&result[..status_end]) {
        return None;
    }
    let mut remaining = &result[status_end + 1..];
    let mut output = CanonicalOutput::default();
    if remaining == b"(no output)\n" {
        output.finish();
        return Some(output);
    }
    let mut parsed_any = false;
    for stream in [Stream::Stdout, Stream::Stderr] {
        let (open, close): (&[u8], &[u8]) = match stream {
            Stream::Stdout => (b"<stdout>\n", b"\n</stdout>\n"),
            Stream::Stderr => (b"<stderr>\n", b"\n</stderr>\n"),
        };
        if !remaining.starts_with(open) {
            continue;
        }
        let body_and_tail = &remaining[open.len()..];
        let close_start = body_and_tail
            .windows(close.len())
            .position(|v| v == close)?;
        output.append(stream, &body_and_tail[..close_start]);
        remaining = &body_and_tail[close_start + close.len()..];
        parsed_any = true;
    }
    if !parsed_any || !remaining.is_empty() {
        return None;
    }
    output.finish();
    Some(output)
}

pub fn valid_foreground_status(status: &[u8]) -> bool {
    if status == b"process finished" {
        return true;
    }
    for prefix in [b"exit_code=".as_slice(), b"signal=".as_slice()] {
        if let Some(number) = status.strip_prefix(prefix) {
            return std::str::from_utf8(number)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .is_some();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    fn normalize(chunks: &[&[u8]]) -> CanonicalOutput {
        let mut out = CanonicalOutput::default();
        for chunk in chunks {
            out.append(Stream::Stdout, chunk);
        }
        out.finish();
        out
    }

    #[test]
    fn normalization_is_chunk_independent() {
        let input = b"progress 1\rprogress 2\rDONE\n\n\x1b[31mred\x1b[0m\tutf8:\xe2\x82\xac invalid:\xff literal:\\x0d";
        let expected = normalize(&[input]);
        for split in 0..=input.len() {
            assert_eq!(expected, normalize(&[&input[..split], &input[split..]]));
        }
        assert_eq!(
            expected
                .records
                .iter()
                .map(|r| r.text.as_slice())
                .collect::<Vec<_>>(),
            vec![
                b"DONE".as_slice(),
                b"",
                b"red\tutf8:\xe2\x82\xac invalid:\\xff literal:\\x0d"
            ]
        );
    }
    #[test]
    fn canonical_output_keeps_first_record_stream_order_across_callbacks() {
        let mut output = CanonicalOutput::default();
        output.append(Stream::Stdout, b"A");
        output.append(Stream::Stderr, b"B\n");
        output.append(Stream::Stdout, b"\n");
        output.finish();
        assert_eq!(output.records.len(), 2);
        assert_eq!(output.records[0].stream, Stream::Stdout);
        assert_eq!(output.records[0].text, b"A");
        assert_eq!(output.records[1].stream, Stream::Stderr);
    }
    #[test]
    fn pending_decoder_bytes_reserve_cross_stream_record_order() {
        let mut output = CanonicalOutput::default();
        output.append(Stream::Stdout, b"\xe2");
        output.append(Stream::Stderr, b"err\n");
        output.append(Stream::Stdout, b"\x82\xac\n");
        output.finish();
        assert_eq!(output.records[0].stream, Stream::Stdout);
        assert_eq!(output.records[0].text, "€".as_bytes());
        assert_eq!(output.records[1].stream, Stream::Stderr);
    }
    #[test]
    fn ansi_only_unterminated_callback_is_removed() {
        let mut output = CanonicalOutput::default();
        output.append(Stream::Stdout, b"\x1b[31m");
        output.finish();
        assert!(output.records.is_empty());
    }
    #[test]
    fn foreground_extraction_is_exact() {
        let live = normalize(&[b"one\n"]);
        let extracted =
            canonicalize_foreground_result(b"exit_code=0\n<stdout>\none\n</stdout>\n").unwrap();
        assert!(eql(&live, &extracted));
        assert!(canonicalize_foreground_result(
            b"exit_code=0\n<stdout>\nliteral\n</stdout>\ntail\n</stdout>\n"
        )
        .is_none());
        assert!(
            canonicalize_foreground_result(b"timeout=true\ncleanup_guarantee=best_effort\n")
                .is_none()
        );
        assert!(valid_foreground_status(b"signal=-9"));
        assert!(!valid_foreground_status(b"exit_code=zero"));
    }
    #[test]
    fn equality_distinguishes_transforms_from_literal_projections() {
        for (raw, projected) in [
            (b"\x00\n".as_slice(), b"\\x00\n".as_slice()),
            (b"\xff\n", b"\\xff\n"),
            (b"old\rnew\n", b"new\n"),
        ] {
            assert_ne!(normalize(&[raw]), normalize(&[projected]));
        }
        assert!(eql(&normalize(&[b"\\x00\n"]), &normalize(&[b"\\x00"])));
    }
    #[test]
    fn fuzz_command_output_content_corpus() {
        for input in [
            b"progress\roverwrite\n".as_slice(),
            b"\x1b[31mred\x1b[0m\n",
            b"exit_code=0\n<stdout>\none\n</stdout>\n",
            b"\xff\x00\xe2\x82",
        ] {
            let split = input.len() / 2;
            let _ = normalize(&[&input[..split], &input[split..]]);
            let _ = canonicalize_foreground_result(input);
        }
    }
}
