# Derive attribute compiler contracts

These fixtures compile complete consumer code without the concrete engine crate. Positive cases verify that rejection tests are not passing because of missing support types. Runtime engine behavior is covered separately by integration tests and the quickstart.

Unknown actor option must not reset the whole actor configuration.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!(#[ParrotActor(engine = "thread", asyn_handler = true)]);
# }
```

An engine literal of the wrong type is an error.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!(#[ParrotActor(engine = 17)]);
# }
```

Duplicate actor options across attributes are errors.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!(#[ParrotActor(engine = "thread")] #[ParrotActor(engine = "actix")]);
# }
```

Invalid actor configuration type syntax must not become EmptyConfig.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!(#[ParrotActor(config = "Vec<")]);
# }
```

Unknown engines are rejected (an already enforced control case).

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!(#[ParrotActor(engine = "unknown")]);
# }
```

Unknown message options must not erase result or validation options.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(result = "u32", validate = "false", timout = 5)]);
# }
```

Invalid result type syntax must not become unit.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(result = "Vec<")]);
# }
```

A misspelled named priority is an error.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(priority = "HGIH")]);
# }
```

Out of range priority is a compile error, not a debug-only runtime panic.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(priority = 101)]);
# }
```

Unknown retry strategies must not silently become Fixed.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(retry_max_attempts = 3, retry_strategy = "Exponental")]);
# }
```

Retry settings without an attempt count must not be silently discarded.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(retry_interval = 2)]);
# }
```

A timeout of the wrong type must not disable configured metadata.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(timeout = "bad")]);
# }
```

Duplicate message options are errors.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(timeout = 1, timeout = 2)]);
# }
```

Malformed validation syntax is a compile error.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(validate = "self.")]);
# }
```

No actor options selects the documented default context and configuration.

```rust
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!();
fn config<A: parrot_api::actor::Actor<Config = parrot_api::actor::EmptyConfig>>() {}
config::<Sample>();
# }
```

A valid thread shape, explicit config and async flag remain usable.

```rust
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
# #[derive(Default)] struct Config;
# impl parrot_api::actor::ActorConfig for Config {}
actor_case!(#[ParrotActor(engine = "thread", config = "Config", async_handler = true)]);
assert!(parrot_api::actor::Actor::use_async_handler(&Sample));
fn config<A: parrot_api::actor::Actor<Config = Config>>() {}
config::<Sample>();
# }
```

Valid message metadata preserves the declared result, bounds, validation and retry strategy.

```rust
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
message_case!(#[message(result = "u32", priority = 100, validate = "false", timeout = 2, retry_max_attempts = 3, retry_strategy = "Linear")]);
assert!(Sample::new(Sample).is_err());
assert_eq!(Sample::extract_result(Box::new(7u32)).unwrap(), 7);
assert_eq!(Sample.priority().value(), 100);
let options = Sample.message_options().unwrap();
assert_eq!(options.timeout, Some(std::time::Duration::from_secs(2)));
assert!(matches!(options.retry_policy.unwrap().backoff_strategy, parrot_api::message::BackoffStrategy::Linear));
# }
```

Actor-generated standard types must not resolve to names declared by the caller.

```rust
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
struct Box;
struct Option;
actor_case!(#[ParrotActor(engine = "actix")]);
assert!(matches!(parrot_api::actor::Actor::state(&Sample), parrot_api::actor::ActorState::Running));
# }
```

Message-generated types must not resolve to caller names or require importing the trait.

```rust
struct Box;
struct Result;
struct Option;
#[derive(parrot_api_derive::Message)]
#[message(result = "u32")]
struct Sample;
fn main() {
    let msg = Sample::new(Sample).unwrap();
    let envelope = msg.into_envelope();
    assert!(envelope.message::<Sample>().is_some());
}
```

Message generics and where clauses must be carried into both generated implementations.

```rust
#[derive(parrot_api_derive::Message)]
#[message(result = "T")]
struct Generic<T>(T) where T: Send + 'static;
fn main() {
    let msg = Generic::new(Generic(7u32)).unwrap();
    assert_eq!(msg.0, 7);
    assert_eq!(<Generic<u32> as parrot_api::Message>::extract_result(Box::new(11u32)).unwrap(), 11);
    let envelope = msg.into_envelope();
    assert_eq!(envelope.message::<Generic<u32>>().unwrap().0, 7);
}
```
Unwired actor supervision options must not silently suggest protection.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!(#[ParrotActor(supervision = "OneForOne")]);
# }
```

Unwired dispatcher attributes must not silently select a different runtime policy.

```compile_fail
# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ui/attribute_support.rs"));
# fn main() {
actor_case!(#[ParrotActor(dispatcher = "dedicated")]);
# }
```

Custom priority constants enforce the same upper bound when used.

```compile_fail
const CUSTOM_PRIORITY: u8 = 101;
#[derive(parrot_api_derive::Message)]
#[message(priority = CUSTOM_PRIORITY)]
struct Sample;
fn main() {
    let _ = parrot_api::Message::priority(&Sample);
}
```

Valid custom priority constants and the lower literal boundary remain supported.

```rust
const CUSTOM_PRIORITY: u8 = 75;
#[derive(parrot_api_derive::Message)]
#[message(priority = CUSTOM_PRIORITY)]
struct Custom;
#[derive(parrot_api_derive::Message)]
#[message(priority = 0)]
struct Lowest;
fn main() {
    assert_eq!(parrot_api::Message::priority(&Custom).value(), 75);
    assert_eq!(parrot_api::Message::priority(&Lowest).value(), 0);
}
```

Raw identifiers naming custom priority constants are valid Rust identifiers.

```rust
#[allow(non_upper_case_globals)]
const r#type: u8 = 25;
#[derive(parrot_api_derive::Message)]
#[message(priority = r#type)]
struct Raw;
fn main() {
    assert_eq!(parrot_api::Message::priority(&Raw).value(), 25);
}
```
