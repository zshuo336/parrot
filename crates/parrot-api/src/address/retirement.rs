use super::ActorRef;
use crate::{
    errors::ActorError,
    owned_input::{call, Input},
    types::{ActorResult, BoxedFuture},
};
use std::task::Poll;

/// Nonblocking observation of one original logical actor's owned cleanup.
/// A completed error is a real retirement failure, including a failure arriving
/// after a public stop timeout. Pending is not proof of successful retirement.
#[derive(Debug)]
pub enum RetirementStatus {
    /// This reference provides no independent retirement receipt.
    Unsupported,
    /// The lifecycle owner has not yet published its actual cleanup result.
    Pending,
    /// The owner has completed cleanup and retained its actual result.
    Retired(ActorResult<()>),
}

/// Request stop and confirm cleanup using the original reference's receipt.
///
/// Unlike `ActorRef::stop`, this explicit cleanup retry may confirm later actual
/// success after a historical timeout. It does not rewrite the public stop
/// result. Unsupported references retain the legacy stop/ActorNotFound fallback;
/// supported pending or failed cleanup cannot be inferred successful from it.
/// Construction, polling, future disposal, observation and discarded errors have
/// separate unwind boundaries. Cancellation does not cancel engine-owned work.
pub fn stop_for_cleanup(actor: &dyn ActorRef) -> BoxedFuture<'_, ActorResult<()>> {
    stop_with_policy(actor, true)
}

/// Confirm retirement without treating an unsupported missing identity as success.
///
/// Supported references use their actual result, including late success after a
/// public timeout. Unsupported references must successfully complete stop; an
/// ActorNotFound error alone cannot prove retirement of an owned hook target.
/// The same independent callback/disposal boundaries as `stop_for_cleanup` apply.
pub fn stop_for_owned_cleanup(actor: &dyn ActorRef) -> BoxedFuture<'_, ActorResult<()>> {
    stop_with_policy(actor, false)
}

fn stop_with_policy(actor: &dyn ActorRef, allow_missing: bool) -> BoxedFuture<'_, ActorResult<()>> {
    Box::pin(crate::trace::capture(async move {
        let mut future = Input::new(call(|| actor.stop())?);
        let mut poll_failed = false;
        let result =
            std::future::poll_fn(|cx| match call(|| future.value_mut().as_mut().poll(cx)) {
                Ok(poll) => poll,
                Err(error) => {
                    poll_failed = true;
                    Poll::Ready(Err(error))
                }
            })
            .await;
        let mut stopped = Input::new(result);
        let disposal = future.retire();
        if poll_failed {
            return Input::new(disposal).finish(stopped.take());
        }
        if let Err(error) = disposal {
            return stopped.finish(Err(error));
        }
        let status = call(|| actor.retirement_status())?;
        let actual = match status {
            RetirementStatus::Retired(result) => result,
            RetirementStatus::Pending => match stopped.take() {
                Ok(()) | Err(ActorError::ActorNotFound(_)) => Err(ActorError::InternalError(
                    "actor retirement is still pending".into(),
                )),
                result => result,
            },
            RetirementStatus::Unsupported => match stopped.take() {
                Ok(()) => Ok(()),
                Err(ActorError::ActorNotFound(_)) if allow_missing => Ok(()),
                result => result,
            },
        };
        stopped.finish(actual)
    }))
}
