use parrot_api::types::{BoxedMessage, BoxedFuture, ActorResult};
 
pub type ThreadActorResult<T> = ActorResult<T>; // Alias for consistency
// Add other common types or constants needed across the thread module here 