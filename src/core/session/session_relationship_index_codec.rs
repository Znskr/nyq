//! Binary codec for the paged subagent relationship index.
//!
//! Mirrors `zig-reference/src/core/session/session_relationship_index_codec.zig`.

use super::session_layout;

#[allow(non_upper_case_globals)]
pub const header_magic: &[u8; 8] = b"FXRELH01";
#[allow(non_upper_case_globals)]
pub const page_magic: &[u8; 8] = b"FXRELP01";
const LEGACY_SCHEMA_VERSION: u32 = 1;
const EPOCH_SCHEMA_VERSION: u32 = 2;
const HEADER_SCHEMA_VERSION: u32 = 3;

#[allow(non_upper_case_globals)]
pub const page_slots: usize = 64;
#[allow(non_upper_case_globals)]
pub const no_slot: u64 = u64::MAX;
#[allow(non_upper_case_globals)]
pub const max_header_bytes: usize = 1024;
#[allow(non_upper_case_globals)]
pub const max_page_bytes: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    OutOfMemory,
    InvalidIndex,
}

pub type Result<T> = std::result::Result<T, Error>;

#[repr(u8)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PendingKind {
    #[default]
    none = 0,
    allocate = 1,
    free = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub storage_epoch: u64,
    pub generation: u64,
    pub high_watermark: u64,
    pub active_count: u64,
    pub active_count_known: bool,
    pub free_head: u64,
    pub delivery_offset: u64,
    pub migration_inode: u64,
    pub migration_size: u64,
    pub migration_mtime_ns: i128,
    pub migration_offset: u64,
    pub pending_kind: PendingKind,
    pub pending_slot: u64,
    pub pending_next_free: u64,
    pub pending_child_len: u16,
    pub pending_child: [u8; 255],
}

impl Default for Header {
    fn default() -> Self {
        Self {
            storage_epoch: 0,
            generation: 0,
            high_watermark: 0,
            active_count: 0,
            active_count_known: true,
            free_head: no_slot,
            delivery_offset: 0,
            migration_inode: 0,
            migration_size: 0,
            migration_mtime_ns: 0,
            migration_offset: 0,
            pending_kind: PendingKind::none,
            pending_slot: no_slot,
            pending_next_free: no_slot,
            pending_child_len: 0,
            pending_child: [0; 255],
        }
    }
}

impl Header {
    #[allow(non_snake_case)]
    pub fn pendingChild(&self) -> &[u8] {
        &self.pending_child[..usize::from(self.pending_child_len)]
    }

    #[allow(non_snake_case)]
    pub fn setPendingChild(&mut self, child_id: &[u8]) {
        assert!(child_id.len() <= self.pending_child.len());
        self.pending_child_len = child_id.len() as u16;
        self.pending_child[..child_id.len()].copy_from_slice(child_id);
    }

    #[allow(non_snake_case)]
    pub fn clearPending(&mut self) {
        self.pending_kind = PendingKind::none;
        self.pending_slot = no_slot;
        self.pending_next_free = no_slot;
        self.pending_child_len = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Slot {
    pub occupied: bool,
    pub next_free: u64,
    pub child_len: u16,
    pub child: [u8; 255],
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            occupied: false,
            next_free: no_slot,
            child_len: 0,
            child: [0; 255],
        }
    }
}

impl Slot {
    #[allow(non_snake_case)]
    pub fn childId(&self) -> &[u8] {
        &self.child[..usize::from(self.child_len)]
    }

    #[allow(non_snake_case)]
    pub fn setOccupied(&mut self, child_id: &[u8]) {
        assert!(child_id.len() <= self.child.len());
        self.occupied = true;
        self.next_free = no_slot;
        self.child_len = child_id.len() as u16;
        self.child[..child_id.len()].copy_from_slice(child_id);
    }

    #[allow(non_snake_case)]
    pub fn setFree(&mut self, next_free: u64) {
        self.occupied = false;
        self.next_free = next_free;
        self.child_len = 0;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageData {
    pub number: u64,
    pub storage_epoch: u64,
    pub slots: [Slot; page_slots],
}

impl PageData {
    pub fn init(number: u64, storage_epoch: u64) -> Self {
        Self {
            number,
            storage_epoch,
            slots: [Slot::default(); page_slots],
        }
    }
}

#[allow(non_snake_case)]
pub fn encodeHeader(header: Header) -> Result<Vec<u8>> {
    assert!(header.active_count_known);
    let encoded_len = 119 + usize::from(header.pending_child_len);
    let mut out = Vec::new();
    out.try_reserve_exact(encoded_len)
        .map_err(|_| Error::OutOfMemory)?;
    out.extend_from_slice(header_magic);
    push_u32(&mut out, HEADER_SCHEMA_VERSION);
    push_u64(&mut out, header.storage_epoch);
    push_u64(&mut out, header.generation);
    push_u64(&mut out, header.high_watermark);
    push_u64(&mut out, header.active_count);
    push_u64(&mut out, header.free_head);
    push_u64(&mut out, header.delivery_offset);
    push_u64(&mut out, header.migration_inode);
    push_u64(&mut out, header.migration_size);
    out.extend_from_slice(&header.migration_mtime_ns.to_le_bytes());
    push_u64(&mut out, header.migration_offset);
    out.push(header.pending_kind as u8);
    push_u64(&mut out, header.pending_slot);
    push_u64(&mut out, header.pending_next_free);
    push_u16(&mut out, header.pending_child_len);
    out.extend_from_slice(header.pendingChild());
    Ok(out)
}

#[allow(non_snake_case)]
pub fn decodeHeader(bytes: &[u8]) -> Result<Header> {
    let mut cursor = ByteCursor::new(bytes);
    if cursor.take(header_magic.len())? != header_magic {
        return Err(Error::InvalidIndex);
    }
    let version = cursor.read_u32()?;
    if !matches!(
        version,
        LEGACY_SCHEMA_VERSION | EPOCH_SCHEMA_VERSION | HEADER_SCHEMA_VERSION
    ) {
        return Err(Error::InvalidIndex);
    }
    let storage_epoch = if version >= EPOCH_SCHEMA_VERSION {
        cursor.read_u64()?
    } else {
        0
    };
    let generation = cursor.read_u64()?;
    let high_watermark = cursor.read_u64()?;
    let active_count = if version == HEADER_SCHEMA_VERSION {
        cursor.read_u64()?
    } else {
        0
    };
    let mut header = Header {
        storage_epoch,
        generation,
        high_watermark,
        active_count,
        active_count_known: version == HEADER_SCHEMA_VERSION,
        free_head: cursor.read_u64()?,
        delivery_offset: cursor.read_u64()?,
        migration_inode: cursor.read_u64()?,
        migration_size: cursor.read_u64()?,
        migration_mtime_ns: cursor.read_i128()?,
        migration_offset: cursor.read_u64()?,
        ..Header::default()
    };
    header.pending_kind = match cursor.read_byte()? {
        0 => PendingKind::none,
        1 => PendingKind::allocate,
        2 => PendingKind::free,
        _ => return Err(Error::InvalidIndex),
    };
    header.pending_slot = cursor.read_u64()?;
    header.pending_next_free = cursor.read_u64()?;
    let child_len = cursor.read_u16()?;
    if usize::from(child_len) > header.pending_child.len() {
        return Err(Error::InvalidIndex);
    }
    let child = cursor.take(usize::from(child_len))?;
    header.setPendingChild(child);

    if !cursor.done()
        || (header.active_count_known && header.active_count > header.high_watermark)
        || header.delivery_offset > header.high_watermark
        || header.migration_offset > header.migration_size
    {
        return Err(Error::InvalidIndex);
    }
    if header.pending_kind == PendingKind::none {
        if header.pending_slot != no_slot
            || header.pending_next_free != no_slot
            || header.pending_child_len != 0
        {
            return Err(Error::InvalidIndex);
        }
    } else if session_layout::validate_session_id(header.pendingChild()).is_err()
        || header.pending_slot == no_slot
    {
        return Err(Error::InvalidIndex);
    }
    Ok(header)
}

#[allow(non_snake_case)]
pub fn encodePage(page_data: PageData) -> Result<Vec<u8>> {
    let encoded_len = 28
        + page_data
            .slots
            .iter()
            .map(|slot| 11 + usize::from(slot.child_len))
            .sum::<usize>();
    let mut out = Vec::new();
    out.try_reserve_exact(encoded_len)
        .map_err(|_| Error::OutOfMemory)?;
    out.extend_from_slice(page_magic);
    push_u32(&mut out, EPOCH_SCHEMA_VERSION);
    push_u64(&mut out, page_data.number);
    push_u64(&mut out, page_data.storage_epoch);
    for slot in page_data.slots {
        out.push(u8::from(slot.occupied));
        push_u64(&mut out, slot.next_free);
        push_u16(&mut out, slot.child_len);
        out.extend_from_slice(slot.childId());
    }
    Ok(out)
}

#[allow(non_snake_case)]
pub fn decodePage(
    bytes: &[u8],
    expected_number: u64,
    expected_storage_epoch: u64,
) -> Result<PageData> {
    let mut page = PageData::init(expected_number, expected_storage_epoch);
    decodePageInto(&mut page, bytes, expected_number, expected_storage_epoch)?;
    Ok(page)
}

#[inline(never)]
#[allow(non_snake_case)]
pub fn decodePageInto(
    page: &mut PageData,
    bytes: &[u8],
    expected_number: u64,
    expected_storage_epoch: u64,
) -> Result<()> {
    let mut cursor = ByteCursor::new(bytes);
    if cursor.take(page_magic.len())? != page_magic {
        return Err(Error::InvalidIndex);
    }
    let version = cursor.read_u32()?;
    if !matches!(version, LEGACY_SCHEMA_VERSION | EPOCH_SCHEMA_VERSION) {
        return Err(Error::InvalidIndex);
    }
    let number = cursor.read_u64()?;
    if number != expected_number {
        return Err(Error::InvalidIndex);
    }
    let storage_epoch = if version == EPOCH_SCHEMA_VERSION {
        cursor.read_u64()?
    } else {
        0
    };
    if storage_epoch != expected_storage_epoch {
        return Err(Error::InvalidIndex);
    }

    page.number = number;
    page.storage_epoch = storage_epoch;
    for slot in &mut page.slots {
        *slot = Slot::default();
        let occupied = match cursor.read_byte()? {
            0 => false,
            1 => true,
            _ => return Err(Error::InvalidIndex),
        };
        let next_free = cursor.read_u64()?;
        let child_len = cursor.read_u16()?;
        if usize::from(child_len) > slot.child.len() {
            return Err(Error::InvalidIndex);
        }
        let child_id = cursor.take(usize::from(child_len))?;
        if occupied {
            if session_layout::validate_session_id(child_id).is_err() || next_free != no_slot {
                return Err(Error::InvalidIndex);
            }
            slot.setOccupied(child_id);
        } else {
            if child_len != 0 {
                return Err(Error::InvalidIndex);
            }
            slot.setFree(next_free);
        }
    }
    if !cursor.done() {
        return Err(Error::InvalidIndex);
    }
    Ok(())
}

#[allow(non_snake_case)]
pub fn pageFileName(number: u64) -> [u8; 38] {
    let mut buffer = [0; 38];
    buffer[..18].copy_from_slice(b"relationship-page-");
    for (index, byte) in number.to_be_bytes().iter().enumerate() {
        buffer[18 + index * 2] = hex_digit(byte >> 4);
        buffer[19 + index * 2] = hex_digit(byte & 0x0f);
    }
    buffer[34..].copy_from_slice(b".bin");
    buffer
}

fn hex_digit(value: u8) -> u8 {
    b"0123456789abcdef"[usize::from(value)]
}

struct ByteCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(len).ok_or(Error::InvalidIndex)?;
        if end > self.bytes.len() {
            return Err(Error::InvalidIndex);
        }
        let result = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(result)
    }

    fn read_byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn read_i128(&mut self) -> Result<i128> {
        Ok(i128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    fn done(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relationship_index_header_preserves_active_occupancy_and_reads_legacy_counts_as_unknown() {
        let bytes = encodeHeader(Header {
            storage_epoch: 7,
            generation: 9,
            high_watermark: 4,
            active_count: 2,
            ..Header::default()
        })
        .unwrap();
        let decoded = decodeHeader(&bytes).unwrap();
        assert!(decoded.active_count_known);
        assert_eq!(2, decoded.active_count);
    }
}
