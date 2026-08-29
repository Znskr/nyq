//! Translation of the binary tape reader in `core/workspace/record_tape.zig`.

pub const MAGIC: &[u8] = b"FXTP\x01";
const HEADER_LEN: usize = MAGIC.len() + 2 + 2 + 8 + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stdout,
    Stdin,
    Resize,
    Sigint,
    Marker,
    Unknown(u8),
}

impl From<u8> for Kind {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::Stdout,
            2 => Self::Stdin,
            3 => Self::Resize,
            4 => Self::Sigint,
            5 => Self::Marker,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReplayHeader<'a> {
    pub cols: u16,
    pub rows: u16,
    pub epoch_ms: i64,
    pub version: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReplayFrame<'a> {
    pub delta_ms: i32,
    pub kind: Kind,
    pub payload: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    TapeTooShort,
    BadTapeMagic,
    TruncatedVersion,
    TruncatedFrameHeader,
    TruncatedFramePayload,
}

pub struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    pub header: ReplayHeader<'a>,
}

impl<'a> Parser<'a> {
    pub fn init(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::TapeTooShort);
        }
        if &bytes[..MAGIC.len()] != MAGIC {
            return Err(Error::BadTapeMagic);
        }
        let mut index = MAGIC.len();
        let cols = u16::from_le_bytes([bytes[index], bytes[index + 1]]);
        index += 2;
        let rows = u16::from_le_bytes([bytes[index], bytes[index + 1]]);
        index += 2;
        let epoch_ms = i64::from_le_bytes(bytes[index..index + 8].try_into().unwrap());
        index += 8;
        let version_len = bytes[index] as usize;
        index += 1;
        let end = index + version_len;
        if end > bytes.len() {
            return Err(Error::TruncatedVersion);
        }
        Ok(Self {
            bytes,
            pos: end,
            header: ReplayHeader {
                cols,
                rows,
                epoch_ms,
                version: &bytes[index..end],
            },
        })
    }

    pub fn next(&mut self) -> Result<Option<ReplayFrame<'a>>, Error> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        if self.pos + 9 > self.bytes.len() {
            return Err(Error::TruncatedFrameHeader);
        }
        let start = self.pos;
        let delta_ms = i32::from_le_bytes(self.bytes[start..start + 4].try_into().unwrap());
        let kind = Kind::from(self.bytes[start + 4]);
        let payload_len =
            u32::from_le_bytes(self.bytes[start + 5..start + 9].try_into().unwrap()) as usize;
        let payload_start = start + 9;
        let payload_end = payload_start
            .checked_add(payload_len)
            .ok_or(Error::TruncatedFramePayload)?;
        if payload_end > self.bytes.len() {
            return Err(Error::TruncatedFramePayload);
        }
        self.pos = payload_end;
        Ok(Some(ReplayFrame {
            delta_ms,
            kind,
            payload: &self.bytes[payload_start..payload_end],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version: &[u8]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&80u16.to_le_bytes());
        bytes.extend_from_slice(&24u16.to_le_bytes());
        bytes.extend_from_slice(&123i64.to_le_bytes());
        bytes.push(version.len() as u8);
        bytes.extend_from_slice(version);
        bytes
    }

    fn frame(bytes: &mut Vec<u8>, delta: i32, kind: u8, payload: &[u8]) {
        bytes.extend_from_slice(&delta.to_le_bytes());
        bytes.push(kind);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
    }

    #[test]
    fn header_and_frames_round_trip() {
        let mut bytes = header(b"vtest");
        frame(&mut bytes, 7, 1, b"hello");
        frame(&mut bytes, 0, 200, b"x");
        let mut parser = Parser::init(&bytes).unwrap();
        assert_eq!(parser.header.cols, 80);
        assert_eq!(parser.header.version, b"vtest");
        assert_eq!(parser.next().unwrap().unwrap().kind, Kind::Stdout);
        assert_eq!(parser.next().unwrap().unwrap().kind, Kind::Unknown(200));
        assert!(parser.next().unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(matches!(Parser::init(b"bad"), Err(Error::TapeTooShort)));
        let mut bytes = header(b"");
        bytes[0] = b'X';
        assert!(matches!(Parser::init(&bytes), Err(Error::BadTapeMagic)));
        let mut bytes = header(b"");
        bytes[HEADER_LEN - 1] = 4;
        assert!(matches!(Parser::init(&bytes), Err(Error::TruncatedVersion)));
    }

    #[test]
    fn detects_truncated_frames() {
        let mut bytes = header(b"");
        bytes.extend_from_slice(&[0, 1, 2]);
        let mut parser = Parser::init(&bytes).unwrap();
        assert_eq!(parser.next(), Err(Error::TruncatedFrameHeader));
        let mut bytes = header(b"");
        frame(&mut bytes, 0, 1, b"ab");
        let len = bytes.len();
        bytes[len - 1] = 0;
        bytes.truncate(len - 1);
        let mut parser = Parser::init(&bytes).unwrap();
        assert_eq!(parser.next(), Err(Error::TruncatedFramePayload));
    }
}
