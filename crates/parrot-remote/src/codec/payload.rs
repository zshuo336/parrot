//! Checked reads for application protocols with fixed little-endian fields.

/// A malformed payload or a limit exceeded before allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PayloadError {
    #[error("truncated application payload")]
    Truncated,
    #[error("application payload contains invalid UTF-8")]
    InvalidUtf8,
    #[error("application payload has trailing bytes")]
    TrailingBytes,
    #[error("application payload exceeds its byte or item budget")]
    BudgetExceeded,
    #[error("minimum item size must be positive")]
    InvalidItemSize,
}

/// Borrowed reader for fixed little-endian integers and u32-prefixed UTF-8.
///
/// It does not allocate. Call [`Self::read_count`] before allocating an item
/// collection, then [`Self::finish`] before accepting the decoded message.
pub struct PayloadReader<'a> {
    remaining: &'a [u8],
}
impl<'a> PayloadReader<'a> {
    /// Bound the entire input before reading or allocating application values.
    pub fn new(input: &'a [u8], max_bytes: usize) -> Result<Self, PayloadError> {
        if input.len() > max_bytes {
            return Err(PayloadError::BudgetExceeded);
        }
        Ok(Self { remaining: input })
    }
    fn take(&mut self, size: usize) -> Result<&'a [u8], PayloadError> {
        let (value, tail) = self
            .remaining
            .split_at_checked(size)
            .ok_or(PayloadError::Truncated)?;
        self.remaining = tail;
        Ok(value)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], PayloadError> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }
    /// Read a little-endian u16.
    pub fn read_u16(&mut self) -> Result<u16, PayloadError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    /// Read a little-endian u32.
    pub fn read_u32(&mut self) -> Result<u32, PayloadError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    /// Read a little-endian u64.
    pub fn read_u64(&mut self) -> Result<u64, PayloadError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    /// Read an exact u32 byte length and borrow its validated UTF-8 string.
    pub fn read_utf8(&mut self) -> Result<&'a str, PayloadError> {
        let size = self.read_u32()? as usize;
        std::str::from_utf8(self.take(size)?).map_err(|_| PayloadError::InvalidUtf8)
    }
    /// Validate a u32 count against both an item limit and the remaining bytes.
    /// `min_item_bytes` is the protocol's minimum encoded size of one item.
    pub fn read_count(
        &mut self,
        max_items: usize,
        min_item_bytes: usize,
    ) -> Result<usize, PayloadError> {
        if min_item_bytes == 0 {
            return Err(PayloadError::InvalidItemSize);
        }
        let count = self.read_u32()? as usize;
        if count > max_items {
            return Err(PayloadError::BudgetExceeded);
        }
        if count > self.remaining.len() / min_item_bytes {
            return Err(PayloadError::Truncated);
        }
        Ok(count)
    }
    /// Reject any bytes not consumed by the application schema.
    pub fn finish(self) -> Result<(), PayloadError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(PayloadError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn count_is_bounded_by_actual_bytes_and_explicit_policy() {
        let bytes = [2, 0, 0, 0, 10, 20];
        let mut reader = PayloadReader::new(&bytes, 6).unwrap();
        assert_eq!(reader.read_count(2, 1), Ok(2));
        let mut reader = PayloadReader::new(&bytes, 6).unwrap();
        assert_eq!(reader.read_count(1, 1), Err(PayloadError::BudgetExceeded));
        let mut reader = PayloadReader::new(&bytes, 6).unwrap();
        assert_eq!(reader.read_count(2, 2), Err(PayloadError::Truncated));
        assert!(matches!(
            PayloadReader::new(&bytes, 5),
            Err(PayloadError::BudgetExceeded)
        ));
        assert_eq!(
            PayloadReader::new(&bytes, 6).unwrap().read_count(2, 0),
            Err(PayloadError::InvalidItemSize)
        );
    }
    #[test]
    fn no_truncated_integer_or_string_is_accepted() {
        let bytes = [
            1, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, b'o', b'k',
        ];
        let decode = |bytes: &[u8]| -> Result<(), PayloadError> {
            let mut r = PayloadReader::new(bytes, 64)?;
            assert_eq!(r.read_u16()?, 1);
            assert_eq!(r.read_u32()?, 2);
            assert_eq!(r.read_u64()?, 3);
            assert_eq!(r.read_utf8()?, "ok");
            r.finish()
        };
        for end in 0..bytes.len() {
            assert_eq!(decode(&bytes[..end]), Err(PayloadError::Truncated));
        }
        assert_eq!(decode(&bytes), Ok(()));
        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(PayloadError::TrailingBytes));
    }
}
