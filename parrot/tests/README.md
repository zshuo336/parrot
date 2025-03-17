# Parrot Actor Framework Tests

This directory contains tests for the Parrot Actor Framework. The tests are designed to cover code and exercise all major functionalities of the library.

## Test Files

- **test_actor_basics.rs**: Tests basic actor creation and message handling without macros.
- **test_actor_macro.rs**: Tests actors and messages created using the `ParrotActor` and `Message` macros.
- **test_async_message.rs**: Tests asynchronous message handling patterns.
- **test_sync_message.rs**: Tests synchronous (immediate) message handling patterns.
- **test_system.rs**: Tests actor system operations, including multiple systems, broadcasting, and system management.
- **test_helpers.rs**: Common utilities used across test files.
- **integration_test.rs**: A single test that runs all the individual tests.

## Running Tests

You can run all tests at once using:

```bash
cargo test --test integration_test
```

Or run an individual test file using:

```bash
cargo run --bin test_actor_basics
cargo run --bin test_actor_macro
cargo run --bin test_async_message
cargo run --bin test_sync_message
cargo run --bin test_system
```

## Test Coverage

These tests aim to achieve over 95% code coverage by exercising:

1. **Manual Actor Creation**: Testing basic actors created manually.
2. **Macro-based Actor Creation**: Testing actors created with the `ParrotActor` derive macro.
3. **Message Types**: Testing all message patterns (request-response, fire-and-forget).
4. **Asynchronous Patterns**: Testing async message handling with callbacks and futures.
5. **System Management**: Testing actor system lifecycle and configuration.
6. **Actor References**: Testing actor path resolution and actor lookup.
7. **Broadcast Messaging**: Testing system-wide message broadcasting.

## Debugging

If a test fails, you can run it individually with more detailed output:

```bash
RUST_BACKTRACE=1 cargo run --bin test_actor_basics
``` 