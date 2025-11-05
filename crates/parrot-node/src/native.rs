//! Explicit authority to execute trusted native code, independent of Admin ACLs.

/// Maximum digests accepted by one node's immutable native policy.
pub const MAX_TRUSTED_DIGESTS: usize = crate::configuration::MAX_NATIVE_TRUST_DIGESTS;

/// Native execution authority. Default denies all native artifacts. This type
/// has no deserializer or public fields: ordinary business/node configuration
/// cannot manufacture authorization. Clone preserves the original trust grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeTrust {
    digests: Vec<String>,
}
impl NativeTrust {
    /// Authorize precisely these SHA-256 digests, independently of their URI.
    /// Hex case and the optional `sha256:` prefix are normalized; duplicates
    /// share one grant. The policy cannot be expanded after node startup.
    ///
    /// # Safety
    /// For every future load authorized by this value, the caller guarantees
    /// the full `parrot_app::host::ThreadPropsSpawner::with_trusted_dylib`
    /// contract: native code and dependencies are trusted, ABI pointers/buffers
    /// are valid, hooks do not unwind or retain borrowed buffers, each instance
    /// permits independent destruction, and no thread/callback/TLS destructor
    /// refers to unloaded code. Digest verification does not prove any of this.
    /// The obligation continues while this value or a configured owner lives.
    ///
    /// ```compile_fail
    /// let _ = parrot_node::NativeTrust::authorize(&["0".repeat(64)]);
    /// ```
    pub unsafe fn authorize(digests: &[String]) -> Result<Self, String> {
        Ok(Self {
            digests: crate::configuration::normalize_native_digests(digests)?,
        })
    }

    /// Whether the node is authorized to accept any native artifact.
    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    pub(crate) fn configure(
        &self,
        mut spawner: parrot_app::host::ThreadPropsSpawner,
    ) -> Result<parrot_app::host::ThreadPropsSpawner, String> {
        for digest in &self.digests {
            // SAFETY: only the unsafe authorize constructor can add a digest.
            spawner = unsafe { spawner.with_trusted_dylib(digest) }.map_err(|e| e.to_string())?;
        }
        Ok(spawner)
    }
}
