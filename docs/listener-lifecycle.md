# Zenoh SHM listener quiescence

The SHM selected-wire core has an independent admission gate per listener.
`ZenohZeroCopyCoreBuilder::with_listener_quiesce_timeout` sets its positive,
representable deadline interval; default is five seconds. The classic
`UPTransportZenoh` listener registry is separate and retains its existing behavior.

Unregister removes the registration, closes admission, and waits outside the map
lock for entered callbacks, unless the current callback is re-entrant. User
callbacks run in spawned async SDK dispatch scopes. Closed registrations stop
scheduling new user deliveries. Subscriber retirement is bounded by the same
deadline from admission close.

Expiry is `DeadlineExceeded` with `QuiesceTimedOut`; it is not a successful drain.
An entered callback/sample retains its owner until completion or cancellation.
The Zenoh sample/SHM handle in each frame keeps retained payload storage valid;
unregister does not invalidate a loan held by the application. This is separate
from queue bounds, delivery guarantees and application-held loan lifetime.

Real SHM tests must exercise normal, self-unregister and timeout/retained-sample
paths. Operational cards freeze the deadline before acceptance, not after failures.
