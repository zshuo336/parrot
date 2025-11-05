//! A type-erased message in one allocation containing a header and its payload.
//!
//! This saves one allocation compared with a genuinely double-boxed envelope.
//! Coercing `Box<M>` to `Box<dyn Any + Send>` itself does not allocate again.
//! Header storage is still allocated for a zero-sized payload. The payload may
//! perform its own allocations; those are outside this envelope's guarantee.
#![deny(unsafe_op_in_unsafe_fn)]

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::any::{Any, TypeId};
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};

#[repr(C)]
struct Header {
    type_id: TypeId,
    drop_fn: unsafe fn(*mut u8),
    payload_off: usize,
    layout: Layout,
}

/// An owning, single-allocation message envelope. Construction accepts only
/// `Send + 'static` payloads. Sharing by reference is intentionally unsupported:
/// a `Send` payload need not be `Sync`.
///
/// ```compile_fail
/// fn require_sync<T: Sync>() {}
/// require_sync::<erased_alloc::SingleAllocEnvelope>();
/// ```
pub struct SingleAllocEnvelope {
    ptr: NonNull<u8>,
}

/// Owns the raw block after its payload is moved out or begins destruction.
/// Keeping this guard on the stack also frees the block if payload Drop unwinds.
struct Allocation {
    ptr: NonNull<u8>,
    layout: Layout,
}
impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns an allocation made with this exact
        // layout. Header is nonzero-sized, even when the payload is a ZST.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

unsafe fn drop_payload<M>(p: *mut u8) {
    // SAFETY: only called from the header paired with an initialized M, once.
    unsafe { ptr::drop_in_place(p.cast::<M>()) };
}

impl SingleAllocEnvelope {
    /// Allocate header, alignment padding, and the moved payload together.
    /// Layout overflow panics before allocation; allocation failure follows
    /// Rust's allocator error handler, just as Box does.
    pub fn new<M: Any + Send>(msg: M) -> Self {
        let (layout, payload_off) = Layout::new::<Header>()
            .extend(Layout::new::<M>())
            .expect("envelope layout exceeds addressable allocation size");
        let layout = layout.pad_to_align();
        // SAFETY: Layout::extend checked size and alignment; Header is nonzero.
        let raw = unsafe { alloc(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        // SAFETY: the block has sufficient size/alignment for both objects.
        // These writes cannot unwind; ownership of msg transfers into the block.
        unsafe {
            ptr::write(
                ptr.as_ptr().cast::<Header>(),
                Header {
                    type_id: TypeId::of::<M>(),
                    drop_fn: drop_payload::<M>,
                    payload_off,
                    layout,
                },
            );
            ptr::write(ptr.as_ptr().add(payload_off).cast::<M>(), msg);
        }
        Self { ptr }
    }

    fn header(&self) -> &Header {
        // SAFETY: all live envelopes own a fully initialized immutable header.
        unsafe { &*self.ptr.as_ptr().cast::<Header>() }
    }

    /// Recover the payload without another allocation. A mismatched type
    /// returns the original envelope unchanged, retaining ownership of its M.
    pub fn downcast<M: Any + Send>(self) -> Result<M, Self> {
        if self.type_id() != TypeId::of::<M>() {
            return Err(self);
        }
        let this = ManuallyDrop::new(self);
        let header = this.header();
        let allocation = Allocation {
            ptr: this.ptr,
            layout: header.layout,
        };
        // SAFETY: TypeId matches M, the header supplies its aligned offset, and
        // self is consumed. ManuallyDrop prevents destruction of the moved M.
        let value = unsafe { ptr::read(this.ptr.as_ptr().add(header.payload_off).cast::<M>()) };
        drop(allocation);
        Ok(value)
    }

    pub fn type_id(&self) -> TypeId {
        self.header().type_id
    }
}

impl Drop for SingleAllocEnvelope {
    fn drop(&mut self) {
        let header = self.header();
        let _allocation = Allocation {
            ptr: self.ptr,
            layout: header.layout,
        };
        // SAFETY: the header and payload were initialized together, the drop
        // function matches M, and &mut self has exclusive ownership. The guard
        // deallocates after this call, including when M::drop panics.
        unsafe { (header.drop_fn)(self.ptr.as_ptr().add(header.payload_off)) };
    }
}

// SAFETY: constructor requires M: Send, no borrowed payload escapes, and the
// allocation, including its destructor, transfers with its exclusive owner.
unsafe impl Send for SingleAllocEnvelope {}

/// A real double-boxed comparison. The erased object's concrete type is `Box<M>`:
/// recover it with `downcast::<Box<M>>()`. Nonzero-sized M costs two allocations;
/// ZST M costs only the outer pointer-sized allocation. This is a comparison
/// model, not evidence that every production message path double-boxes.
pub fn two_alloc_baseline<M: Any + Send>(msg: M) -> Box<dyn Any + Send> {
    Box::new(Box::new(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BigMsg {
        _data: [u64; 32], // 256B，确保跨对齐边界
        tag: u64,
    }

    #[test]
    fn single_alloc_roundtrip() {
        let env = SingleAllocEnvelope::new(BigMsg {
            _data: [0; 32],
            tag: 42,
        });
        assert_eq!(env.type_id(), TypeId::of::<BigMsg>());
        let m = env.downcast::<BigMsg>().ok().expect("type match");
        assert_eq!(m.tag, 42);
    }

    #[test]
    fn single_alloc_wrong_type_keeps_envelope() {
        let env = SingleAllocEnvelope::new(1234u64);
        let back = env.downcast::<String>().expect_err("must fail");
        // 原 envelope 仍可用
        let m = back.downcast::<u64>().ok().unwrap();
        assert_eq!(m, 1234);
    }

    #[test]
    fn drop_runs_payload_destructor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Tracked;
        impl Drop for Tracked {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        let before = DROPS.load(Ordering::SeqCst);
        {
            let env = SingleAllocEnvelope::new(Tracked);
            drop(env);
        }
        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            before + 1,
            "payload Drop 必须执行"
        );
    }
}
