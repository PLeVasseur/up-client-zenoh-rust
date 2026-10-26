# up-transport-zenoh-rust

This crate implements the Zenoh transport as specified in [uProtocol v1.6.0-alpha.7](https://github.com/eclipse-uprotocol/up-spec/blob/v1.6.0-alpha.7/up-l1/zenoh.adoc).

Native prepared applications use `ZenohPreparedCore` with a finite route catalog,
an eagerly initialized SHM provider and generic SDK registrations. Metadata and
payload remain in the same native SHM chunk; publication is synchronous and each
native callback pushes directly into its SDK inbox. See the
[prepared SHM guide](docs/prepared.md) for capacities, placement, allocation
observations and native ownership.
`from_session` permits shared native-session setup while keeping each core's
provider and finite route/registration capacities explicit.
The `zero-copy` feature enables the SDK's inline URI storage. Default classic
builds retain its String-backed URI storage; owned support does not require SHM.

`ZenohZeroCopyCore` is a dynamic allocating SHM control with attachment-based
metadata; its feature-gated selected-wire wrapper uses the owned support adapter.
Its bounded retirement interval defaults to five seconds. See
[listener lifecycle](docs/listener-lifecycle.md) for that separate control contract.

## Build

The [benchmark guide](docs/benchmarks.md) documents the Criterion wrapper,
full-loop versus component selectors and the blocked aggregate-comparison result.

Public items and targets use `missing_docs = "deny"`. See the
[public API guide](docs/api-contract.md) for builder, RPC and retained-buffer
contracts, with links to this binding's placement and lifecycle guides.
SDK-facing frame/message priorities share `UPriority`, and checked projections
preserve each sample's encoding and payload presence. The metadata codec supplies
layout identity; prepared SHM views retain their actual native sample without
payload copying. An optional setup-installed allocation observer scopes our code
and suspends only native middleware calls, re-entering every callback. Whole-process
counts include middleware and observer costs; they are reported separately.

```shell
# Check clippy
cargo clippy --all-targets
# Build
cargo build
# Run test
cargo test
# Test coverage
cargo tarpaulin -o lcov -o html --output-dir target/tarpaulin
```

## Examples

The examples of up-transport-zenoh-rust can be found under examples folder.

```shell
# Publisher
cargo run --example publisher
# Subscriber
cargo run --example subscriber
# RPC Server
cargo run --example rpc_server
# RPC Client
cargo run --example rpc_client
```

## Note

The implementation follows the spec defined in [up-l1/zenoh](https://github.com/eclipse-uprotocol/up-spec/blob/main/up-l1/zenoh.adoc).

## Usage
Both libraries need to be added as dependencies to your crate, e.g. using the following commands:

```sh
cargo add up-rust
cargo add up-transport-zenoh
```

Please refer to the [publisher](examples/publisher.rs) and [subscriber](examples/subscriber.rs) examples to see how to initialize and use the transport.

### Supported Service Classes
`uman~supported-service-classes~1`

The Zenoh transport supports all service classes defined by uProtocol and maps them to corresponding Zenoh message priority levels:

| uProtocol Service Class | Zenoh Priority Level |
| :---------------------- | :------------------- |
| `CS0`                   | `Background`         |
| `CS1`                   | `DataLow`            |
| `CS2`                   | `Data`               |
| `CS3`                   | `DataHigh`           |
| `CS4`                   | `InteractiveLow`     |
| `CS5`                   | `InteractiveHigh`    |
| `CS6`                   | `RealTime`           |

Covers:
- `req~utransport-send-qos-mapping~1`

### Supported Message Delivery Methods
`uman~supported-message-delivery-methods~1`

The transport provided by this crate supports the [push delivery method](https://github.com/eclipse-uprotocol/up-spec/blob/v1.6.0-alpha.7/up-l1/README.adoc#5-message-delivery) only.
The `UPTransportZenoh::receive` function therefore always returns `UCode::UNIMPLEMENTED`.

Covers:
- `req~utransport-delivery-methods~1`

### Authentication & Authorization
`uman~auth-configuration~1`

The transport provided by this crate can be configured with credentials that the transport will provide to the Zenoh router during connection establishment. A [_username_ and _password_](https://zenoh.io/docs/manual/user-password/) can be specified in the Zenoh configuration that is passed into the `UPTransportZenohBuilder::with_config` or `UPTransportZenohBuilder::with_config_path` functions.

Access to resources can be configured in the Zenoh (peer's or router's) configuration by means of [Access Control Lists](https://zenoh.io/docs/manual/access-control/).
The [authorization integration tests](./tests/authorization.rs) illustrate, how ACLs can be used to restrict a client's authority to put and subscribe to messages using corresponding rule sets.

Covers:
- `req~utransport-send-prevent-address-spoofing~1`
- `req~utransport-registerlistener-prevent-unauthorized-access~1`

### Maximum number of listeners
`uman~max-listeners-configuration~1`

The transport provided by this crate supports setting an upper limit to the number of (filter pattern, listener) tuples that can be registered by means of the `UPTransportZenohBuilder::with_max_listeners` function. Please refer to the [API Documentation](https://docs.rs/up-transport-zenoh/) for details.

Covers:
- `req~utransport-registerlistener-max-listeners~1`

## Design

### Message Delivery
`dsn~supported-message-delivery-methods~1`

All messages are being received by means of registering callbacks for relevant Zenoh key patterns and delivering the messages to listeners that have been registered via `UPTransportZenoh::register_listener`.
The callbacks dispatch all incoming messages to the registered listeners on the _same_ thread that the Zenoh runtime runs on.

Rationale:
The Zenoh protocol does not provide means to poll other nodes for messages but only supports the push model by means of clients subscribing to key patterns.

The transport requires a tokio runtime to execute but does not make any implicit assumption regarding the availability and size of thread pools. This provides for flexibility regarding the environment that the transport can be deployed to but also requires application developers to take care when implementing message listeners, making sure to not block the transport's message callback when processing a dispatched message.

Covers:
- `req~utransport-delivery-methods~1`

Needs: impl, itest

### Authorization
`dsn~utransport-authorization~1`

In general, uProtocol entities are only allowed to send messages on their own behalf. Certain specific uEntities acting as a uProtocol Streamer also need to send messages _on behalf of_ other uEntities in order to fulfill their original purpose of routing messages hence and forth between different transports. Making these authoritzation decisions requires the (proven) establishment of an _identity_ and its _authorities_.

The Zenoh transport delegates all authorization decisions to the Zenoh router (or peer) that the transport is configured to connect to. For this purpose, the transport supports configuration of credentials which are being used during connection establishment. The Zenoh router uses the provided credentials to establish the client's identity and its associated authorities. Whenever the uEntity sends a message via the router or registers a subscriber for a key pattern, the router verifies that the client is authorized to publish using the key or receive messages matching the key pattern.

Rationale:
The Zenoh transport is implemented as a library that is linked to the (custom) code that implements a uEntity's functionality. It is therefore not feasible to perform the authentication and authoritzation within the transport library code, because uEntities can not be forced to actually utilize one of uProtocol's transport libraries but may instead chooose to implement the binding to the transport protocol themselves.

Covers:
- `req~utransport-send-prevent-address-spoofing~1`
- `req~utransport-registerlistener-prevent-unauthorized-access~1`

Needs: itest

## Change Log

Please refer to the [Releases on GitHub](https://github.com/eclipse-uprotocol/up-transport-zenoh-rust/releases) for the change log.

[^1]: Some PC configurations cannot connect locally. Add multicast to `lo` interface using
  ` $ sudo ip link set dev lo multicast on `

## Feature-Gated Owned Support

`ZenohOwnedCore` is available only with `--features benchmark-owned`. It is a disabled-by-default owned-frame support path for benchmark/support measurements and selected-wire owned tests; it is not zero-copy evidence and is not part of the default transport API.
Native selected-wire publishers may opt into bounded matching-subscriber readiness
with `ZenohZeroCopyCoreBuilder::with_publisher_readiness`. See
[publisher readiness](docs/publisher-readiness.md) for timeout, cancellation and
presence-versus-delivery semantics; SHM allocation and listener quiescence remain
separate binding contracts.

Native selected-wire cores may opt into construction-time provider initialization
with `ZenohZeroCopyCoreBuilder::with_shm_prewarm(true)`. The default remains lazy.
See [SHM provider prewarming](docs/shm-prewarming.md) for native mapping/locking,
setup failures and the distinction between configured size and measured residency.
## Ordered classic subscriber dispatch

Classic and owned subscribers use one setup-owned generic worker and a one-credit queue.
Owned duplicate registrations share one pending/native registration. Owned pull
receive completes one operation through a nonblocking one-shot slot.
Dynamic SHM control registration also owns pending setup explicitly, and its pull
receive retains only the first result's native lease. Transport destruction closes
admission before native listener retirement.
Synchronous publication back into an entered local native sink is refused before
native put; registration-worker handlers remain the forwarding path.
Local callback-originated sends and RPC queries reserve matching queue credit
across transports sharing a session before native
publication or return `ResourceExhausted`; unregister closes credit before drain.
See [listener lifecycle](docs/listener-lifecycle.md) for ordering, reentry and the
separate native SHM ownership contract.
Classic request queryables share the registered listener's worker. Their query
tickets roll back on failed or cancelled delivery; one setup-owned expiry task
retires request correlations at their TTL, and unregister clears its queries.
Responses must match the request's endpoints before consuming its native query.
The RPC client validates response type, request ID and endpoints, preserves absent
and present-empty payloads, and uses the original request deadline for reply waiting.
Its one-shot reply slot accepts the first native response without blocking and
discards duplicate responses.
Dynamic SHM
sinks account for entered native callbacks when waiting for quiescence.
