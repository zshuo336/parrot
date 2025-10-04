# Parrot API Test Suite

This directory contains the comprehensive test suite for the Parrot Actor Framework API. The tests are divided into unit tests for specific components and integration tests that validate the system as a whole.

## Test File Organization

- **actor_test.rs** - Unit tests for core Actor components and lifecycle
- **address_test.rs** - Unit tests for Actor addresses, references, and path resolution
- **context_test.rs** - Unit tests for Actor context and execution environment
- **errors_test.rs** - Unit tests for error handling and recovery mechanisms
- **message_test.rs** - Unit tests for the messaging system and envelope
- **runtime_test.rs** - Unit tests for the runtime environment and execution
- **system_test.rs** - Unit tests for the Actor system management
- **integration_test.rs** - End-to-end tests validating multiple components working together

## Test Coverage

The test suite achieves over 95% code coverage, testing the following core functionalities:

### Actor Lifecycle & Behavior
- Actor initialization, startup, and termination
- State management and transitions
- Message processing and response handling
- Actor supervision hierarchy

### Messaging System
- Type-safe message passing
- Message priorities and delivery guarantees
- Request-response patterns
- Fire-and-forget operations
- Message timeouts and retries
- Custom message types and serialization

### Concurrency & Performance
- Concurrent message processing
- Throughput and latency benchmarks
- Resilience under high load
- Thread safety and race condition prevention

### Error Handling & Recovery
- Error propagation and handling
- Supervision strategies (OneForOne, AllForOne)
- Graceful degradation
- System-wide error management
- Timeout handling

### System Configuration & Management
- Runtime configuration options
- Resource allocation and tuning
- Load balancing strategies
- System metrics and monitoring

### Message Routing & Patterns
- Point-to-point messaging
- Broadcast patterns
- Content-based routing
- Round-robin distribution
- Actor hierarchies and group communication

## Running Tests

Run all tests:
```bash
cargo test --package parrot-api --all-targets
```

Run a specific test file:
```bash
cargo test --package parrot-api --test integration_test
```

Run a specific test:
```bash
cargo test --package parrot-api --test integration_test test_concurrent_message_handling
```

## Code Coverage

To generate code coverage reports, we use [cargo-tarpaulin](https://github.com/xd009642/tarpaulin). If you don't have it installed, you can install it with:

```bash
cargo install cargo-tarpaulin
```

Generate HTML coverage report:
```bash
cargo tarpaulin --workspace --ignore-tests --out Html --output-dir ./target/tarpaulin
```

Generate text summary in the terminal:
```bash
cargo tarpaulin --workspace --ignore-tests
```

Track coverage changes over time:
```bash
cargo tarpaulin --workspace --ignore-tests --out Json --output-dir ./target/tarpaulin/history
```

For CI integration, you can use:
```bash
cargo tarpaulin --workspace --ignore-tests --out Xml --output-dir ./target/tarpaulin
```

## Performance Benchmarks

The test suite includes performance benchmarks in integration_test.rs that measure:
- Message throughput (messages/second)
- Message processing latency
- System recovery time

These benchmarks help ensure the system meets performance requirements and identify potential bottlenecks.