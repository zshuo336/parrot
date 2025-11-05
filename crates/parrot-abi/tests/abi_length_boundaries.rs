//! Real oversized views: reserve zero-filled virtual memory without touching 4 GiB.
#![cfg(all(target_os = "linux", target_pointer_width = "64"))]

use parrot_abi::{AbiMsg, AbiReplyBuf, AbiStr};

struct Mapping {
    pointer: *mut u8,
    length: usize,
}

impl Mapping {
    fn new(length: usize) -> Self {
        // Anonymous mmap supplies initialized zero bytes. NORESERVE avoids
        // reserving physical/swap backing; these tests only inspect view lengths.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(
            pointer,
            libc::MAP_FAILED,
            "virtual reservation failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(!pointer.is_null());
        Self {
            pointer: pointer.cast(),
            length,
        }
    }

    fn bytes(&self) -> &[u8] {
        // The mapping remains live and all anonymous bytes are initialized.
        unsafe { std::slice::from_raw_parts(self.pointer, self.length) }
    }

    /// The caller must ensure that all mapped bytes remain valid UTF-8.
    unsafe fn text(&self) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.bytes()) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // Exclusive borrow of the mapping prevents simultaneous shared views.
        unsafe { std::slice::from_raw_parts_mut(self.pointer, self.length) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        assert_eq!(unsafe { libc::munmap(self.pointer.cast(), self.length) }, 0);
    }
}

#[test]
fn maximum_abi_lengths_keep_the_complete_original_views() {
    let mut mapping = Mapping::new(u32::MAX as usize);
    // Untouched anonymous mappings contain only zero bytes, which are UTF-8.
    let text = AbiStr::of_str(unsafe { mapping.text() });
    assert_eq!(text.ptr, mapping.pointer.cast_const());
    assert_eq!(text.len, u32::MAX);
    let key = AbiMsg::new(unsafe { mapping.text() }, b"payload");
    assert_eq!(key.type_key, mapping.pointer.cast_const());
    assert_eq!(key.key_len, u32::MAX);
    let payload = AbiMsg::new("key", mapping.bytes());
    assert_eq!(payload.payload, mapping.pointer.cast_const());
    assert_eq!(payload.payload_len, u32::MAX);
    let reply = AbiReplyBuf::new(mapping.bytes_mut());
    assert_eq!(reply.buf, mapping.pointer);
    assert_eq!(reply.cap, u32::MAX);
    assert_eq!(reply.written, 0);
}

#[test]
#[should_panic(expected = "ABI string length exceeds u32")]
fn overlong_string_cannot_silently_wrap() {
    let mut mapping = Mapping::new(u32::MAX as usize + 2);
    mapping.bytes_mut()[..2].copy_from_slice(b"\xc3\xa9");
    // One valid two-byte character followed by zeros; a wrapped length of one
    // would split that character. Never dereference the old malformed view.
    let _ = AbiStr::of_str(unsafe { mapping.text() });
}

#[test]
#[should_panic(expected = "ABI message key length exceeds u32")]
fn overlong_message_key_cannot_silently_wrap() {
    let mut mapping = Mapping::new(u32::MAX as usize + 2);
    mapping.bytes_mut()[..2].copy_from_slice(b"\xc3\xa9");
    // The prefix is valid UTF-8 and all remaining bytes are zeros.
    let _ = AbiMsg::new(unsafe { mapping.text() }, b"payload");
}

#[test]
#[should_panic(expected = "ABI message payload length exceeds u32")]
fn overlong_message_payload_cannot_silently_wrap() {
    let mapping = Mapping::new(u32::MAX as usize + 1);
    let _ = AbiMsg::new("key", mapping.bytes());
}

#[test]
#[should_panic(expected = "ABI reply capacity exceeds u32")]
fn overlong_reply_capacity_cannot_silently_wrap() {
    let mut mapping = Mapping::new(u32::MAX as usize + 1);
    let _ = AbiReplyBuf::new(mapping.bytes_mut());
}
