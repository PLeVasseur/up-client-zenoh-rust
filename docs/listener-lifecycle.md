# Zenoh SHM listener quiescence

The dynamic allocating SHM control has an independent admission gate per sink.
`ZenohZeroCopyCoreBuilder::with_listener_quiesce_timeout` sets its positive,
representable deadline interval; default is five seconds. The classic
`UPTransportZenoh` listener registry is separate and retains its existing behavior.

Unregister removes the registration, closes admission, and waits outside the map
lock for entered callbacks, unless the current callback is re-entrant. User
callbacks run in their generic SDK registration worker. Native callbacks push
synchronously into that worker's inbox. Closed registrations stop admitting
new user deliveries. Subscriber retirement is bounded by the same
deadline from admission close.

Expiry is `DeadlineExceeded` with `QuiesceTimedOut`; it is not a successful drain.
An entered callback/sample retains its owner until completion or cancellation.
The Zenoh sample/SHM handle in each frame keeps retained payload storage valid;
unregister does not invalidate a loan held by the application. This is separate
from queue bounds, delivery guarantees and application-held loan lifetime.

Real SHM tests must exercise normal, self-unregister and timeout/retained-sample
paths. See [prepared SHM routes](prepared.md) for the separate finite native
subscription/SDK stop/cancel contract and ownership after detach. Physical native
retirement, worker drain/cancellation and retained sample lifetime are distinct.
