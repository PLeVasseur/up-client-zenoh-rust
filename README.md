# Rust based Eclipse Zenoh&trade; Transport Library for Eclipse uProtocol&trade;

This crate implements the Zenoh transport as specified in [uProtocol v1.6.0-alpha.7](https://github.com/eclipse-uprotocol/up-spec/tree/v1.6.0-alpha.7).

## Getting started

### Building the Library

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

### Running the Examples

The [examples](examples) folder contains sample code illustrating how the crate can be used for the different message exchange patterns supported by uProtocol.
Assume you're using debug build.[^1]

```shell
# Owned-frame publisher
./target/debug/examples/owned_publisher
# Owned-frame subscriber
./target/debug/examples/owned_subscriber
# Stable typed Zenoh SHM publisher, requires --features zero-copy
./target/debug/examples/zero_copy_stable_publisher
# Stable typed Zenoh SHM subscriber, requires --features zero-copy
./target/debug/examples/zero_copy_stable_subscriber
```

For the advanced Zenoh configuration, you can either use `-h` to see more details or pass the configuration file with `-c`.
The example configuration file is located in the [config folder](config).

## Using the Library

Most developers will want to create an instance of the *UPTransportZenoh* struct and use it as a native owned-frame transport with the Communication Level API provided by the *up-rust* library.

`UPTransportZenoh` implements `UOwnedTransport` in all builds. It preserves native `UFrameMetadata`, including `UAttributes` and `PayloadEncoding`, in the Zenoh attachment while carrying the application payload as Zenoh payload bytes. Standard encodings carry upstream `UPayloadFormat` values; custom encodings carry a native custom ID plus content type.

When the `zero-copy` feature is enabled, `UPTransportZenoh` also implements `up_rust::zero_copy::UZeroCopyTransport` using Zenoh shared-memory payload buffers on transmit and Zenoh `ZBytes` lease views on receive. Transmit loans are requested with `UTxLoanSpec`; metadata is final at `loan_tx`, where the binding maps it to the Zenoh key, priority, and attachment before the caller writes into `ZenohTxBuffer::payload_mut()`.

| uProtocol frame part | Zenoh representation |
| --- | --- |
| `UAttributes.source` / `sink` | Zenoh key expression and attachment metadata |
| `UAttributes.priority` | Zenoh priority |
| `UAttributes` optional fields | Zenoh attachment metadata |
| `PayloadEncoding` | Zenoh attachment metadata |
| Application payload bytes | Zenoh payload or SHM payload bytes |

Owned send helpers serialize application values before handing the frame to Zenoh:

```rust
use up_rust::{payload::RawBytes, transport::UOwnedTransportExt, UFrameMetadata};

async fn send<T>(transport: &T, metadata: UFrameMetadata) -> Result<(), up_rust::UStatus>
where
    T: up_rust::UOwnedTransport,
{
    let payload: &[u8] = b"payload";
    transport
        .send_serialized::<RawBytes, _>(metadata, &payload)
        .await
}
```

Zero-copy send helpers create a Zenoh SHM loan first, then serialize directly into the loan:

```rust
use up_rust::{payload::RawBytes, zero_copy::UZeroCopyTransportExt, UFrameMetadata};

async fn send<T>(transport: &T, metadata: UFrameMetadata) -> Result<(), up_rust::UStatus>
where
    T: up_rust::zero_copy::UZeroCopyTransport,
{
    let payload: &[u8] = b"payload";
    transport
        .send_serialized_zero_copy::<RawBytes, _>(metadata, &payload)
        .await
}
```

Stable typed payloads can be constructed directly in Zenoh SHM without first
materializing or default-initializing an application payload buffer:

```rust
use up_rust::{payload::StableContainerPayload, zero_copy::UZeroCopyUninitTransportExt, UFrameMetadata};

#[repr(C)]
#[derive(Clone, Copy, up_rust::StablePayload, up_rust::ByteBackedStablePayload)]
#[stable_payload(type_name = "example.vehicle.VehiclePose")]
struct VehiclePose {
    x: u64,
    y: u64,
}

async fn send<T>(transport: &T, metadata: UFrameMetadata) -> Result<(), up_rust::UStatus>
where
    T: up_rust::zero_copy::UZeroCopyUninitTransport,
{
    transport
        .send_uninit_loaned_payload_as::<StableContainerPayload<VehiclePose>, VehiclePose>(
            metadata,
            |slot| Ok(slot.write(VehiclePose { x: 1, y: 2 })),
        )
        .await
}
```

On the zero-copy receive path, Zenoh payload bytes must be SHM-backed to qualify
as loan-backed stable payloads. Pull receive returns `FAILED_PRECONDITION` for
non-SHM payload bytes, while listeners drop non-SHM payloads with a warning.
Use the owned transport APIs for interoperable regular Zenoh payload bytes.
Stable-container typed receive uses `borrow_stable_payload<T>()` on the
loan-backed RX lease. The direct stable-container proof is
`send_uninit_loaned_payload_as::<StableContainerPayload<T>, T>` on TX followed by
`receive_zero_copy` and `borrow_stable_payload<T>()` on RX; non-SHM `ZBytes`
payloads are not treated as strict zero-copy receive.

Conformance coverage for the native-frame path includes attachment metadata
round trips, standard and custom payload encoding preservation, payload/encoding
mismatch rejection, present-empty versus absent payload handling, and strict
rejection of non-SHM payloads on loan-backed stable-container receive.

The zero-copy builder uses a 64 MiB Zenoh SHM provider segment by default.
`UPTransportZenohBuilder::with_shm_segment_size(size)?` can override it and
rejects `0` with `INVALID_ARGUMENT` before any SHM provider is initialized.

Both libraries need to be added as dependencies to your crate, e.g. using the following commands:

```sh
cargo add up-rust
cargo add up-transport-zenoh
```

Please refer to the [owned publisher](examples/owned_publisher.rs) and [owned subscriber](examples/owned_subscriber.rs) examples to see how to initialize and use the transport.

### Supported Service Classes
`uman~supported-service-classes~1`

The Zenoh transport supports all service classes defined by uProtocol and maps them to corresponding Zenoh message priority levels.

Covers:
- `req~utransport-send-qos-mapping~1`

### Supported Message Delivery Methods
`uman~supported-message-delivery-methods~1`

The transport provided by this crate supports the [push delivery method](https://github.com/eclipse-uprotocol/up-spec/blob/v1.6.0-alpha.7/up-l1/README.adoc#5-message-delivery) only.
The `UPTransportZenoh::receive_owned` function therefore always returns `UCode::UNIMPLEMENTED`.

Covers:
- `req~utransport-delivery-methods~1`

### Authentication & Authorization
`uman~auth-configuration~1`

The transport provided by this crate can be configured with credentials that the transport will provide to the Zenoh router during connection establishment. A [_username_ and _password_](https://zenoh.io/docs/manual/user-password/) can be specified in the Zenoh config file that is passed into the `UPTransportZenohBuilder::with_config_file` function.

Access to resources can be configured in the Zenoh (peer's or router's) config file by means of [Access Control Lists](https://zenoh.io/docs/manual/access-control/).
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

All messages are being received by means of registering callbacks for relevant Zenoh key patterns and delivering the messages to listeners that have been registered via `UPTransportZenoh::register_owned_listener`.
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
