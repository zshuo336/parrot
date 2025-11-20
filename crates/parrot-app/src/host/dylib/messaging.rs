use super::*;
use parrot_abi::*;
use parrot_remote::codec_registry::CodecRegistry;
use std::{collections::BTreeSet, sync::Weak, time::Duration};

const MAX_CODEC_BYTES: usize = 1024;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMessageGrant {
    pub source_path: String,
    pub artifact_digest: String,
    pub destination: String,
    pub request_type_key: String,
    pub operation: NativeMessageOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NativeMessageOperation {
    Ask { reply_type_key: String },
    Tell,
}
impl<'de> serde::Deserialize<'de> for NativeMessageOperation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
        enum Operation {
            Ask { reply_type_key: String },
            Tell {},
        }
        Ok(match Operation::deserialize(deserializer)? {
            Operation::Ask { reply_type_key } => Self::Ask { reply_type_key },
            Operation::Tell {} => Self::Tell,
        })
    }
}

pub(super) fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= MAX_PATH_BYTES
        && !path
            .chars()
            .any(|character| character.is_control() || matches!(character, '?' | '#'))
        && path[1..]
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
fn valid_destination(path: &str) -> bool {
    if valid_path(path) {
        return true;
    }
    let Some(rest) = path.strip_prefix("parrot://") else {
        return false;
    };
    let Some((node, _)) = rest.split_once('/') else {
        return false;
    };
    path.len() <= MAX_PATH_BYTES
        && parrot_config::endpoint::validate_node_id(node).is_ok()
        && valid_path(&rest[node.len()..])
}
fn valid_key(key: &str) -> bool {
    key.len() <= MAX_CODEC_BYTES && parrot_remote::codec::CodecStack::of_type_key(key).is_ok()
}
pub(super) fn validate(grants: &mut [NativeMessageGrant]) -> Result<(), DeployError> {
    if grants.len() > 256 {
        return Err(DeployError::Artifact(
            "at most 256 native messaging grants".into(),
        ));
    }
    let mut signatures = BTreeSet::new();
    for grant in grants {
        grant.artifact_digest = digest_hex(&grant.artifact_digest)?;
        if !valid_path(&grant.source_path)
            || !valid_destination(&grant.destination)
            || grant.source_path == grant.destination
            || !valid_key(&grant.request_type_key)
            || matches!(&grant.operation, NativeMessageOperation::Ask { reply_type_key } if !valid_key(reply_type_key))
        {
            return Err(DeployError::Artifact(
                "invalid native messaging grant".into(),
            ));
        }
        if !signatures.insert((
            grant.source_path.clone(),
            grant.artifact_digest.clone(),
            grant.destination.clone(),
            grant.request_type_key.clone(),
            matches!(grant.operation, NativeMessageOperation::Ask { .. }),
        )) {
            return Err(DeployError::Artifact(
                "overlapping native messaging grants".into(),
            ));
        }
    }
    Ok(())
}

pub(super) struct Binding {
    engine: Weak<ThreadActorSystem>,
    runtime: tokio::runtime::Handle,
    grants: Vec<NativeMessageGrant>,
}
impl Binding {
    pub(super) fn new(
        engine: &Arc<ThreadActorSystem>,
        source: &str,
        digest: &str,
        grants: &[NativeMessageGrant],
    ) -> Self {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        Self {
            engine: Arc::downgrade(engine),
            runtime: engine.runtime_handle().clone(),
            grants: grants
                .iter()
                .filter(|grant| {
                    grant.source_path == source
                        && grant.artifact_digest.eq_ignore_ascii_case(digest)
                })
                .cloned()
                .collect(),
        }
    }
    #[cfg(test)]
    pub(super) fn session(&self, context: Option<TraceContext>) -> Session<'_> {
        self.session_with_budget(context, budget::Budget::default())
    }
    pub(super) fn session_with_budget(
        &self,
        context: Option<TraceContext>,
        budget: budget::Budget,
    ) -> Session<'_> {
        Session {
            binding: self,
            context,
            budget,
        }
    }
}

pub(super) struct Session<'a> {
    binding: &'a Binding,
    context: Option<TraceContext>,
    budget: budget::Budget,
}
impl Session<'_> {
    pub(super) fn host(&mut self) -> AbiMessagingHostV1 {
        AbiMessagingHostV1 {
            context: std::ptr::from_mut(self).cast(),
            ask,
            tell,
        }
    }
    fn boundary(&mut self, operation: impl FnOnce(&mut Self) -> u32) -> u32 {
        if !self.budget.attempt() {
            return ABI_MESSAGE_LIMIT;
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(self))) {
            Ok(code) => code,
            Err(payload) => {
                self.budget.panic(payload);
                ABI_MESSAGE_PANIC
            }
        }
    }
    pub(super) fn complete(self, result: &mut Result<(), (u32, String)>) {
        self.budget.complete(result);
    }
    fn charge(&mut self, bytes: usize) -> Result<(), u32> {
        if !self.budget.charge(bytes) {
            return Err(ABI_MESSAGE_LIMIT);
        }
        Ok(())
    }
    fn authorize(&self, destination: &str, key: &str, reply: Option<&str>) -> Result<(), u32> {
        self.binding
            .grants
            .iter()
            .find(|grant| {
                grant.destination == destination
                    && grant.request_type_key == key
                    && match (&grant.operation, reply) {
                        (NativeMessageOperation::Ask { reply_type_key }, Some(reply)) => {
                            reply_type_key == reply
                        }
                        (NativeMessageOperation::Tell, None) => true,
                        _ => false,
                    }
            })
            .map(|_| ())
            .ok_or(ABI_MESSAGE_DENIED)
    }
    fn target(
        &self,
        destination: &str,
        key: &str,
        bytes: &[u8],
    ) -> Result<(BoxedActorRef, BoxedMessage), u32> {
        let registry = CodecRegistry::global();
        let message = registry
            .decode_incoming(key, bytes)
            .map_err(|_| ABI_MESSAGE_CODEC)?;
        // An alias for the lifecycle codec must not acquire management authority.
        if message.is::<ParrotAppHook>() {
            return Err(ABI_MESSAGE_DENIED);
        }
        let actor = self
            .binding
            .engine
            .upgrade()
            .and_then(|engine| engine.resolve_actor(destination))
            .ok_or(ABI_MESSAGE_TARGET)?;
        // A fully qualified alias for this node's source actor must not turn
        // a synchronous guest callback into a self-ASK or acquire hook access.
        if self
            .binding
            .grants
            .iter()
            .any(|grant| actor.path() == grant.source_path)
        {
            return Err(ABI_MESSAGE_DENIED);
        }
        Ok((actor, message))
    }
    fn request(
        &mut self,
        destination: &str,
        key: &str,
        payload: &[u8],
        reply_key: Option<&str>,
        timeout: u32,
    ) -> Result<Vec<u8>, u32> {
        if !valid_destination(destination)
            || !valid_key(key)
            || !(1..=30000).contains(&timeout)
            || reply_key.is_some_and(|key| !valid_key(key))
        {
            return Err(ABI_MESSAGE_INVALID);
        }
        self.charge(destination.len() + key.len() + payload.len() + reply_key.map_or(0, str::len))?;
        self.authorize(destination, key, reply_key)?;
        // Native dispatch runs on its own OS thread. The actor engine runtime
        // keeps polling target actors while this thread awaits the bounded call.
        let result = self.binding.runtime.block_on(parrot_api::trace::scope(
            self.context.clone(),
            async {
                tokio::time::timeout(Duration::from_millis(u64::from(timeout)), async {
                    let (actor, message) = self.target(destination, key, payload)?;
                    if let Some(reply_key) = reply_key {
                        let response = actor.send(message).await.map_err(|_| ABI_MESSAGE_FAILED)?;
                        let registry = CodecRegistry::global();
                        if registry.key_of((*response).type_id()) != Some(reply_key) {
                            return Err(ABI_MESSAGE_REPLY);
                        }
                        let (_, bytes) = registry
                            .encode_outgoing(&response)
                            .map_err(|_| ABI_MESSAGE_CODEC)?;
                        if bytes.len() > MAX_MESSAGE_BYTES {
                            return Err(ABI_MESSAGE_LIMIT);
                        }
                        Ok(bytes)
                    } else {
                        actor
                            .deliver(message)
                            .await
                            .map_err(|_| ABI_MESSAGE_FAILED)?;
                        Ok(Vec::new())
                    }
                })
                .await
                .map_err(|_| ABI_MESSAGE_TIMEOUT)?
            },
        ))?;
        self.charge(result.len())?;
        Ok(result)
    }
}

unsafe fn text<'a>(value: AbiStr, max: usize) -> Result<&'a str, u32> {
    if value.ptr.is_null() || value.len == 0 || value.len as usize > max {
        return Err(ABI_MESSAGE_INVALID);
    }
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(value.ptr, value.len as usize) })
        .map_err(|_| ABI_MESSAGE_INVALID)
}
unsafe fn message<'a>(msg: AbiMsg) -> Result<(&'a str, &'a [u8]), u32> {
    let key = unsafe {
        text(
            AbiStr {
                ptr: msg.type_key,
                len: msg.key_len,
            },
            MAX_CODEC_BYTES,
        )
    }?;
    if msg.payload_len as usize > MAX_MESSAGE_BYTES
        || (msg.payload.is_null() && msg.payload_len != 0)
    {
        return Err(ABI_MESSAGE_INVALID);
    }
    let bytes = if msg.payload_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(msg.payload, msg.payload_len as usize) }
    };
    Ok((key, bytes))
}

unsafe extern "C" fn ask(
    context: *mut (),
    destination: AbiStr,
    msg: AbiMsg,
    expected: AbiStr,
    out: *mut AbiReplyBuf,
    timeout: u32,
) -> u32 {
    if !out.is_null() {
        unsafe {
            (*out).written = 0;
        }
    }
    if context.is_null() {
        return ABI_MESSAGE_INVALID;
    }
    let session = unsafe { &mut *context.cast::<Session<'_>>() };
    session.boundary(|session| {
        if out.is_null() {
            return ABI_MESSAGE_INVALID;
        }
        // Live, aligned and disjoint pointers are the trusted ABI contract.
        let out = unsafe { &mut *out };
        out.written = 0;
        if out.cap as usize > MAX_MESSAGE_BYTES || (out.buf.is_null() && out.cap != 0) {
            return ABI_MESSAGE_INVALID;
        }
        let result = (|| {
            let destination = unsafe { text(destination, MAX_PATH_BYTES) }?;
            let (key, payload) = unsafe { message(msg) }?;
            let expected = unsafe { text(expected, MAX_CODEC_BYTES) }?;
            session.request(destination, key, payload, Some(expected), timeout)
        })();
        match result {
            Ok(bytes) => {
                if bytes.len() > out.cap as usize {
                    return ABI_MESSAGE_LIMIT;
                }
                if !bytes.is_empty() {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.buf, bytes.len());
                    }
                }
                out.written = bytes.len() as u32;
                ABI_MESSAGE_OK
            }
            Err(code) => code,
        }
    })
}
unsafe extern "C" fn tell(context: *mut (), destination: AbiStr, msg: AbiMsg, timeout: u32) -> u32 {
    if context.is_null() {
        return ABI_MESSAGE_INVALID;
    }
    let session = unsafe { &mut *context.cast::<Session<'_>>() };
    session.boundary(|session| {
        let result = (|| {
            let destination = unsafe { text(destination, MAX_PATH_BYTES) }?;
            let (key, payload) = unsafe { message(msg) }?;
            session.request(destination, key, payload, None, timeout)
        })();
        result.map_or_else(|code| code, |_| ABI_MESSAGE_OK)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_ffi_views_count_attempts_and_clear_failed_output() {
        let engine = ThreadActorSystem::shared(Default::default());
        let binding = Binding::new(&engine, "/user/source", &"0".repeat(64), &[]);
        let mut session = binding.session(None);
        let host = session.host();
        let mut bytes = [0; 8];
        let mut out = AbiReplyBuf::new(&mut bytes);
        let destination = AbiStr::of_str("/user/target");
        let msg = AbiMsg::new("bin:request#v1", b"");
        let reply = AbiStr::of_str("bin:reply#v1");
        // Oversized/null detection views never reach slice construction.
        unsafe {
            for invalid in [
                AbiStr {
                    ptr: std::ptr::null(),
                    len: 1,
                },
                AbiStr {
                    ptr: b"x".as_ptr(),
                    len: 4097,
                },
                AbiStr {
                    ptr: b"\xff".as_ptr(),
                    len: 1,
                },
            ] {
                out.written = 7;
                assert_eq!(
                    (host.ask)(host.context, invalid, msg, reply, &mut out, 1),
                    ABI_MESSAGE_INVALID
                );
                assert_eq!(out.written, 0);
            }
            for timeout in [0, 30001, u32::MAX] {
                assert_eq!(
                    (host.tell)(host.context, destination, msg, timeout),
                    ABI_MESSAGE_INVALID
                );
            }
            let oversized = AbiMsg {
                payload_len: (MAX_MESSAGE_BYTES + 1) as u32,
                ..msg
            };
            assert_eq!(
                (host.tell)(host.context, destination, oversized, 1),
                ABI_MESSAGE_INVALID
            );
            let null_payload = AbiMsg {
                payload: std::ptr::null(),
                payload_len: 1,
                ..msg
            };
            assert_eq!(
                (host.tell)(host.context, destination, null_payload, 1),
                ABI_MESSAGE_INVALID
            );
            let null_empty = AbiMsg {
                payload: std::ptr::null(),
                payload_len: 0,
                ..msg
            };
            assert_eq!(
                (host.tell)(host.context, destination, null_empty, 1),
                ABI_MESSAGE_DENIED
            );
            assert_eq!(
                (host.ask)(
                    host.context,
                    destination,
                    msg,
                    reply,
                    std::ptr::null_mut(),
                    1
                ),
                ABI_MESSAGE_INVALID
            );
            assert_eq!(session.budget.calls(), 10);
            for _ in 10..64 {
                assert_eq!(
                    (host.tell)(host.context, destination, msg, 1),
                    ABI_MESSAGE_DENIED
                );
            }
            out.written = 7;
            assert_eq!(
                (host.ask)(host.context, destination, msg, reply, &mut out, 1),
                ABI_MESSAGE_LIMIT
            );
            assert_eq!(out.written, 0);
            assert_eq!(session.budget.calls(), 64);
        }
        engine.shutdown_internal().await.unwrap();
    }

    #[tokio::test]
    async fn callback_panic_and_throwing_payload_force_failure_after_guest_success() {
        let engine = ThreadActorSystem::shared(Default::default());
        let binding = Binding::new(&engine, "/user/source", &"0".repeat(64), &[]);
        let mut session = binding.session(None);
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Payload(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("native message payload disposal");
            }
        }
        assert_eq!(
            session.boundary(|_| std::panic::panic_any(Payload(drops.clone()))),
            ABI_MESSAGE_PANIC
        );
        let mut result = Ok(());
        session.complete(&mut result);
        assert_eq!(result.unwrap_err().0, ABI_ERR_PANIC);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        engine.shutdown_internal().await.unwrap();
    }
}
