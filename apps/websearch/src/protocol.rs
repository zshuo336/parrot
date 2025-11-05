use parrot_remote::codec::{PayloadError, PayloadReader};
pub type FrontierEntry = (String, u16);
pub type TermEntry = (String, u64, u32);
const MAX_ITEMS: usize = 65_536;
pub fn decode_batch(bytes: &[u8]) -> Result<Vec<FrontierEntry>, PayloadError> {
    let mut r = PayloadReader::new(bytes, parrot_remote::frame::MAX_FRAME_LEN as usize)?;
    let count = r.read_count(MAX_ITEMS, 6)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((r.read_utf8()?.to_owned(), r.read_u16()?));
    }
    r.finish()?;
    Ok(entries)
}
pub fn decode_terms(bytes: &[u8]) -> Result<Vec<TermEntry>, PayloadError> {
    let mut r = PayloadReader::new(bytes, parrot_remote::frame::MAX_FRAME_LEN as usize)?;
    let count = r.read_count(MAX_ITEMS, 16)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((r.read_utf8()?.to_owned(), r.read_u64()?, r.read_u32()?));
    }
    r.finish()?;
    Ok(entries)
}

// Fixed-width replies use the same complete-consumption rule as batch replies.
pub fn decode_count(bytes: &[u8]) -> Result<u32, PayloadError> {
    let mut r = PayloadReader::new(bytes, 4)?;
    let result = r.read_u32()?;
    r.finish()?;
    Ok(result)
}
pub fn decode_size(bytes: &[u8]) -> Result<(u32, u32), PayloadError> {
    let mut r = PayloadReader::new(bytes, 8)?;
    let result = (r.read_u32()?, r.read_u32()?);
    r.finish()?;
    Ok(result)
}
pub fn decode_stats(bytes: &[u8]) -> Result<(u32, u64), PayloadError> {
    let mut r = PayloadReader::new(bytes, 12)?;
    let result = (r.read_u32()?, r.read_u64()?);
    r.finish()?;
    Ok(result)
}
