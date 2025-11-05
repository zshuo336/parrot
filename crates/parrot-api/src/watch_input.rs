//! Direct watch adapters isolate capability callbacks and each owned input.
use crate::{
    address::ActorRef,
    errors::ActorError,
    owned_input::{call, Input},
    types::{ActorResult, BoxedActorRef, BoxedFuture},
};

pub(crate) fn change<'a, R: ActorRef + ?Sized>(
    target: &'a R,
    watcher: BoxedActorRef,
    remove: bool,
) -> BoxedFuture<'a, ActorResult<()>> {
    // Capture ownership before returning: even a never-polled future must
    // dispose its watcher independently, without unwinding through its caller.
    let watcher = Input::new(watcher);
    Box::pin(async move {
        let result = (|| {
            let target = Input::new(call(|| target.death_watch())?.ok_or_else(|| {
                ActorError::InternalError("target has no death-watch capability".into())
            })?);
            let result = (|| {
                let identity =
                    Input::new(call(|| watcher.value().death_watch())?.ok_or_else(|| {
                        ActorError::InternalError("watcher has no death-watch capability".into())
                    })?);
                let result = call(|| {
                    if remove {
                        target.value().unwatch(identity.value());
                        Ok(())
                    } else {
                        target.value().watch(identity.value())
                    }
                })
                .and_then(|result| result);
                identity.finish(result)
            })();
            target.finish(result)
        })();
        // Keep the first capability/operation error. Only an otherwise successful
        // result becomes a cleanup failure; subscription effects are not undone.
        watcher.finish(result)
    })
}
