pub(super) use crate::owned_payload::OwnedPayload;
use parrot_api::types::{ActorResult, BoxedFuture};

pub(super) struct RejectedSpawn {
    pub error: parrot_api::system::SystemError,
    pub retired: BoxedFuture<'static, ActorResult<()>>,
}
