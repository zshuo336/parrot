# ActixContextExt Implementation Proposal

## Objective
Allow access to the underlying Actix context within ParrotActor's `receive_message` method, enabling users to directly utilize Actix-specific features.

## Current Design
- `ActixContextExt` trait has been defined, providing the `actix_addr` method to obtain the Actix address, and the `with_native_context` method to access the native Actix context
- Currently, `with_native_context` is implemented as empty, returning None

## Challenges
1. **Thread Safety Issues**: Attempting to store Actix context references encounters thread safety problems, as the Actix context is not `Send`
2. **Type System Limitations**: In Rust's type system, there are strict limitations on conversions between generic parameters and associated types
3. **Lifetime Issues**: The lifetime of the Actix context is incompatible with the asynchronous lifetime of message processing
4. **Pointer Safety**: Using raw pointers may lead to memory safety issues

## Considered Solutions

### Solution 1: Thread Local Storage
Attempted to use thread local storage to pass context references, but this is unreliable in asynchronous code as tasks may be scheduled across threads.

### Solution 2: Using Pointers
Attempted to store pointers to the native context in ActixActor, but this leads to thread safety and lifetime issues.

### Solution 3: Special Messages
Attempted to pass context references to the actor through special Actix messages, but problems still exist at asynchronous boundaries.

## Recommended Implementation Approach

Consider the following methods to solve this problem:

1. **Modify Message Flow**: Merge Actix message handling and ParrotActor's `receive_message` into synchronous processing (avoiding asynchronicity), or

2. **Provide Limited API**: Instead of directly exposing the Actix context, design a safe API subset, such as:
   ```rust
   pub trait ActixContextExt<A: actix::Actor> {
       // This already works
       fn actix_addr(&self) -> Addr<A>;
       
       // Add other safe methods
       fn register_stream<S>(&self, stream: S) -> StreamHandler<...>;
       fn schedule_periodic(&self, duration: Duration, f: Box<dyn Fn()>);
       // etc...
   }
   ```

3. **Modify Architecture**: Consider redesigning the integration between Parrot and Actix, allowing users to directly use Actix in certain scenarios while using Parrot abstractions in others.

## Near-term Implementable Solutions

Before fully resolving the issue, we can:

1. Document the current limitations of `with_native_context`
2. Provide example code demonstrating how to meet user needs without using `with_native_context`
3. Offer alternative APIs for users, such as wrapping common Actix functionality into Parrot's API

## Next Steps

1. Discuss design choices with the team
2. Evaluate solutions that best align with the project's overall architecture
3. Create prototypes to validate the selected approach, especially regarding memory and thread safety considerations