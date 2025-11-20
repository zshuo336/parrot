use super::{budget::Budget, *};
use parrot_abi::*;
use parrot_api::receptionist::{
    ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream, RegistrationScope,
};
use std::{collections::BTreeMap, time::Duration};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum NativeServiceOperation {
    /// Publish or withdraw this actor's own registration.
    Register,
    Subscribe,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeServiceGrant {
    pub source_path: String,
    pub artifact_digest: String,
    pub key: String,
    pub operation: NativeServiceOperation,
}
pub(super) fn validate(grants: &mut [NativeServiceGrant]) -> Result<(), DeployError> {
    if grants.len() > 256 {
        return Err(DeployError::Config(
            "at most 256 native service grants".into(),
        ));
    }
    let mut signatures = BTreeSet::new();
    for grant in grants {
        grant.artifact_digest = digest_hex(&grant.artifact_digest)?;
        if !messaging::valid_path(&grant.source_path)
            || grant.key.len() > 1024
            || ReceptionistKey::new(grant.key.clone()).is_err()
        {
            return Err(DeployError::Config(
                "invalid native service grant path or key".into(),
            ));
        }
        if !signatures.insert((
            grant.source_path.clone(),
            grant.artifact_digest.clone(),
            grant.key.clone(),
            grant.operation,
        )) {
            return Err(DeployError::Config(
                "overlapping native service grants".into(),
            ));
        }
    }
    Ok(())
}

pub(super) struct Binding {
    path: String,
    scope: Arc<RegistrationScope>,
    gateway: Option<Arc<dyn ReceptionistGateway>>,
    runtime: tokio::runtime::Handle,
    grants: Vec<NativeServiceGrant>,
}
impl Binding {
    pub(super) fn new<A: Actor + Send + Sync + 'static>(
        ctx: &ThreadContext<A>,
        grants: Vec<NativeServiceGrant>,
    ) -> Self {
        let engine = ctx
            .system_weak_ref()
            .upgrade()
            .expect("admitted native actor engine");
        Self {
            path: ctx.path_str().into(),
            scope: ctx.receptionist_registration_scope(),
            gateway: engine.receptionist_gateway(),
            runtime: engine.runtime_handle().clone(),
            grants,
        }
    }
    fn authorize(
        &self,
        key: AbiStr,
        timeout: u32,
        operation: NativeServiceOperation,
    ) -> Result<ReceptionistKey, u32> {
        if !(1..=30000).contains(&timeout) {
            return Err(ABI_SERVICE_INVALID);
        }
        let key = unsafe { text(key) }?;
        let parsed = ReceptionistKey::new(key).map_err(|_| ABI_SERVICE_INVALID)?;
        if !self
            .grants
            .iter()
            .any(|grant| grant.key == key && grant.operation == operation)
        {
            return Err(ABI_SERVICE_DENIED);
        }
        self.live()?;
        Ok(parsed)
    }
    fn live(&self) -> Result<(), u32> {
        self.scope.with_live(|| ()).map_err(|_| ABI_SERVICE_CLOSED)
    }
}

struct Stream {
    receiver: ReceptionistStream,
    pending: Option<(u32, String)>,
}
pub(super) struct State {
    binding: Binding,
    streams: BTreeMap<u64, Stream>,
    next_handle: u64,
}
impl Drop for State {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
impl State {
    pub(super) fn retire(&mut self) -> Result<(), String> {
        let mut failed = false;
        for stream in std::mem::take(&mut self.streams).into_values() {
            failed |= !crate::disposal::drop_catching(stream);
        }
        if failed {
            Err("native service subscription disposal panicked".into())
        } else {
            Ok(())
        }
    }
    pub(super) fn new(binding: Binding) -> Self {
        Self {
            binding,
            streams: BTreeMap::new(),
            next_handle: 1,
        }
    }
    pub(super) fn session(&mut self, budget: Budget) -> Session<'_> {
        Session {
            state: self,
            budget,
        }
    }
    fn remove(&mut self, handle: u64) -> Result<(), u32> {
        let stream = self.streams.remove(&handle).ok_or(ABI_SERVICE_HANDLE)?;
        // Drop inside the invocation panic boundary; siblings remain owned.
        drop(stream);
        Ok(())
    }
}
pub(super) struct Session<'a> {
    state: &'a mut State,
    budget: Budget,
}
impl Session<'_> {
    pub(super) fn host(&mut self) -> AbiServicesHostV1 {
        AbiServicesHostV1 {
            context: std::ptr::from_mut(self).cast(),
            register,
            deregister,
            subscribe,
            next,
            close,
        }
    }
    fn boundary(&mut self, operation: impl FnOnce(&mut Self) -> Result<(), u32>) -> u32 {
        if !self.budget.attempt() {
            return ABI_SERVICE_LIMIT;
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(self))) {
            Ok(result) => result.map_or_else(|code| code, |()| ABI_SERVICE_OK),
            Err(payload) => {
                self.budget.panic(payload);
                ABI_SERVICE_PANIC
            }
        }
    }
    fn key(
        &self,
        key: AbiStr,
        timeout: u32,
        operation: NativeServiceOperation,
    ) -> Result<ReceptionistKey, u32> {
        let key = self.state.binding.authorize(key, timeout, operation)?;
        if !self.budget.charge(key.as_str().len()) {
            return Err(ABI_SERVICE_LIMIT);
        }
        Ok(key)
    }
    fn publish(&mut self, key: AbiStr, timeout: u32, remove: bool) -> Result<(), u32> {
        let key = self.key(key, timeout, NativeServiceOperation::Register)?;
        let binding = &self.state.binding;
        let gateway = binding.gateway.as_ref().ok_or(ABI_SERVICE_FAILED)?;
        binding.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(timeout.into()), async {
                if remove {
                    gateway
                        .deregister_scoped(key, binding.path.clone(), binding.scope.clone())
                        .await
                } else {
                    gateway
                        .register_scoped(key, binding.path.clone(), binding.scope.clone())
                        .await
                }
                .map_err(|_| ABI_SERVICE_FAILED)
            })
            .await
            .map_err(|_| ABI_SERVICE_TIMEOUT)?
        })
    }
    fn subscribe(&mut self, key: AbiStr, timeout: u32) -> Result<u64, u32> {
        let key = self.key(key, timeout, NativeServiceOperation::Subscribe)?;
        if self.state.streams.len() >= 16 || self.state.next_handle == u64::MAX {
            return Err(ABI_SERVICE_LIMIT);
        }
        let binding = &self.state.binding;
        let gateway = binding.gateway.as_ref().ok_or(ABI_SERVICE_FAILED)?;
        let receiver = binding.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(timeout.into()),
                gateway.subscribe(key),
            )
            .await
            .map_err(|_| ABI_SERVICE_TIMEOUT)?
            .map_err(|_| ABI_SERVICE_FAILED)
        })?;
        binding.live()?;
        let handle = self.state.next_handle;
        self.state.next_handle += 1;
        self.state.streams.insert(
            handle,
            Stream {
                receiver,
                pending: None,
            },
        );
        Ok(handle)
    }
    fn next(
        &mut self,
        handle: u64,
        timeout: u32,
        out: &mut AbiReplyBuf,
        kind: &mut u32,
    ) -> Result<(), u32> {
        if !(1..=30000).contains(&timeout) || out.cap > 4096 || (out.buf.is_null() && out.cap != 0)
        {
            return Err(ABI_SERVICE_INVALID);
        }
        let state = &mut self.state;
        let stream = state.streams.get_mut(&handle).ok_or(ABI_SERVICE_HANDLE)?;
        let binding = &state.binding;
        if binding.live().is_err() {
            state.remove(handle)?;
            return Err(ABI_SERVICE_CLOSED);
        }
        if stream.pending.is_none() {
            let event = binding.runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(timeout.into()), async {
                    tokio::select! {
                        biased;
                        () = binding.scope.closed() => None,
                        event = stream.receiver.recv() => event,
                    }
                })
                .await
                .map_err(|_| ABI_SERVICE_TIMEOUT)
            })?;
            let Some(event) = event else {
                state.remove(handle)?;
                return Err(ABI_SERVICE_CLOSED);
            };
            stream.pending = Some(match event {
                ReceptionistEvent::Registered { remote_path, .. } => {
                    (ABI_SERVICE_REGISTERED, remote_path)
                }
                ReceptionistEvent::Unregistered { remote_path, .. } => {
                    (ABI_SERVICE_UNREGISTERED, remote_path)
                }
            });
        }
        let (event_kind, path) = stream.pending.as_ref().unwrap();
        if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
            state.remove(handle)?;
            return Err(ABI_SERVICE_INVALID);
        }
        if path.len() > out.cap as usize || !self.budget.charge(path.len()) {
            return Err(ABI_SERVICE_LIMIT);
        }
        if !path.is_empty() {
            unsafe {
                std::ptr::copy_nonoverlapping(path.as_ptr(), out.buf, path.len());
            }
        }
        out.written = path.len() as u32;
        *kind = *event_kind;
        stream.pending = None;
        Ok(())
    }
}

unsafe fn text<'a>(key: AbiStr) -> Result<&'a str, u32> {
    if key.ptr.is_null() || key.len == 0 || key.len > 1024 {
        return Err(ABI_SERVICE_INVALID);
    }
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(key.ptr, key.len as usize) })
        .map_err(|_| ABI_SERVICE_INVALID)
}
unsafe fn invoke(
    context: *mut (),
    operation: impl FnOnce(&mut Session<'_>) -> Result<(), u32>,
) -> u32 {
    if context.is_null() {
        return ABI_SERVICE_INVALID;
    }
    unsafe { &mut *context.cast::<Session<'_>>() }.boundary(operation)
}
unsafe extern "C" fn register(context: *mut (), key: AbiStr, timeout: u32) -> u32 {
    unsafe { invoke(context, |session| session.publish(key, timeout, false)) }
}
unsafe extern "C" fn deregister(context: *mut (), key: AbiStr, timeout: u32) -> u32 {
    unsafe { invoke(context, |session| session.publish(key, timeout, true)) }
}
unsafe extern "C" fn subscribe(context: *mut (), key: AbiStr, timeout: u32, out: *mut u64) -> u32 {
    if !out.is_null() {
        unsafe {
            *out = 0;
        }
    }
    unsafe {
        invoke(context, |session| {
            if out.is_null() {
                return Err(ABI_SERVICE_INVALID);
            }
            *out = session.subscribe(key, timeout)?;
            Ok(())
        })
    }
}
unsafe extern "C" fn next(
    context: *mut (),
    handle: u64,
    timeout: u32,
    out: *mut AbiReplyBuf,
    kind: *mut u32,
) -> u32 {
    if !out.is_null() {
        unsafe {
            (*out).written = 0;
        }
    }
    if !kind.is_null() {
        unsafe {
            *kind = 0;
        }
    }
    unsafe {
        invoke(context, |session| {
            if out.is_null() || kind.is_null() {
                return Err(ABI_SERVICE_INVALID);
            }
            session.next(handle, timeout, &mut *out, &mut *kind)
        })
    }
}
unsafe extern "C" fn close(context: *mut (), handle: u64) -> u32 {
    unsafe { invoke(context, |session| session.state.remove(handle)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(engine: &Arc<ThreadActorSystem>, scope: Arc<RegistrationScope>) -> State {
        State::new(Binding {
            path: "/user/native".into(),
            scope,
            gateway: None,
            runtime: engine.runtime_handle().clone(),
            grants: vec![],
        })
    }
    #[tokio::test]
    async fn invalid_ffi_views_charge_budget_and_clear_all_outputs() {
        let engine = ThreadActorSystem::shared(Default::default());
        let mut state = state(&engine, Arc::new(RegistrationScope::default()));
        let budget = Budget::default();
        let mut session = state.session(budget.clone());
        let host = session.host();
        unsafe {
            for invalid in [
                AbiStr {
                    ptr: std::ptr::null(),
                    len: 1,
                },
                AbiStr {
                    ptr: b"x".as_ptr(),
                    len: 1025,
                },
                AbiStr {
                    ptr: b"\xff".as_ptr(),
                    len: 1,
                },
                AbiStr::of_str(""),
            ] {
                assert_eq!(
                    (host.register)(host.context, invalid, 1),
                    ABI_SERVICE_INVALID
                );
            }
            for timeout in [0, 30001, u32::MAX] {
                assert_eq!(
                    (host.register)(host.context, AbiStr::of_str("native/key"), timeout),
                    ABI_SERVICE_INVALID
                );
            }
            let mut handle = 99;
            assert_eq!(
                (host.subscribe)(host.context, AbiStr::of_str("native/key"), 1, &mut handle),
                ABI_SERVICE_DENIED
            );
            assert_eq!(handle, 0);
            assert_eq!(
                (host.subscribe)(
                    host.context,
                    AbiStr::of_str("native/key"),
                    1,
                    std::ptr::null_mut()
                ),
                ABI_SERVICE_INVALID
            );
            let mut bytes = [0; 8];
            let mut out = AbiReplyBuf::new(&mut bytes);
            let mut kind = 88;
            out.cap = 4097;
            assert_eq!(
                (host.next)(host.context, 1, 1, &mut out, &mut kind),
                ABI_SERVICE_INVALID
            );
            assert_eq!((out.written, kind), (0, 0));
            out.cap = 8;
            out.buf = std::ptr::null_mut();
            assert_eq!(
                (host.next)(host.context, 1, 1, &mut out, &mut kind),
                ABI_SERVICE_INVALID
            );
            out.cap = 0;
            assert_eq!(
                (host.next)(host.context, 1, 1, &mut out, &mut kind),
                ABI_SERVICE_HANDLE
            );
            assert_eq!(
                (host.next)(host.context, 1, 1, std::ptr::null_mut(), &mut kind),
                ABI_SERVICE_INVALID
            );
            assert_eq!(
                (host.next)(host.context, 1, 1, &mut out, std::ptr::null_mut()),
                ABI_SERVICE_INVALID
            );
            for _ in budget.calls()..64 {
                assert_eq!((host.close)(host.context, 1), ABI_SERVICE_HANDLE);
            }
            handle = 99;
            assert_eq!(
                (host.subscribe)(host.context, AbiStr::of_str("native/key"), 1, &mut handle),
                ABI_SERVICE_LIMIT
            );
            assert_eq!(handle, 0);
        }
        engine.shutdown_internal().await.unwrap();
    }

    struct Throw(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Throw {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("stream guard drop");
        }
    }
    #[tokio::test]
    async fn each_throwing_stream_is_retired_and_failure_is_reported() {
        let engine = ThreadActorSystem::shared(Default::default());
        let mut state = state(&engine, Arc::new(RegistrationScope::default()));
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for handle in 1..=3 {
            let (_, receiver) = tokio::sync::mpsc::channel(1);
            state.streams.insert(
                handle,
                Stream {
                    receiver: ReceptionistStream::from(receiver)
                        .with_lifetime_guard(Throw(dropped.clone())),
                    pending: None,
                },
            );
        }
        let budget = Budget::default();
        let mut session = state.session(budget.clone());
        let host = session.host();
        assert_eq!(unsafe { (host.close)(host.context, 1) }, ABI_SERVICE_PANIC);
        let mut result = Ok(());
        budget.complete(&mut result);
        assert_eq!(result.unwrap_err().0, ABI_ERR_PANIC);
        assert!(state.retire().is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        assert!(state.streams.is_empty());
        assert!(state.retire().is_ok());
        engine.shutdown_internal().await.unwrap();
    }
    #[tokio::test]
    async fn exhausted_handles_never_wrap_and_closed_generation_drops_pending_events() {
        let engine = ThreadActorSystem::shared(Default::default());
        let scope = Arc::new(RegistrationScope::default());
        let mut state = state(&engine, scope.clone());
        state.binding.grants.push(NativeServiceGrant {
            source_path: "/user/native".into(),
            artifact_digest: "0".repeat(64),
            key: "native/key".into(),
            operation: NativeServiceOperation::Subscribe,
        });
        state.next_handle = u64::MAX;
        let mut session = state.session(Budget::default());
        let host = session.host();
        let mut handle = 99;
        assert_eq!(
            unsafe { (host.subscribe)(host.context, AbiStr::of_str("native/key"), 1, &mut handle) },
            ABI_SERVICE_LIMIT
        );
        assert_eq!(handle, 0);
        let (_, receiver) = tokio::sync::mpsc::channel(1);
        state.streams.insert(
            1,
            Stream {
                receiver: receiver.into(),
                pending: Some((ABI_SERVICE_REGISTERED, "/user/late".into())),
            },
        );
        scope.close();
        let mut session = state.session(Budget::default());
        let host = session.host();
        let mut bytes = [0; 64];
        let mut out = AbiReplyBuf::new(&mut bytes);
        let mut kind = 99;
        assert_eq!(
            unsafe { (host.next)(host.context, 1, 1, &mut out, &mut kind) },
            ABI_SERVICE_CLOSED
        );
        assert_eq!((out.written, kind), (0, 0));
        assert!(state.streams.is_empty());
        engine.shutdown_internal().await.unwrap();
    }
}
