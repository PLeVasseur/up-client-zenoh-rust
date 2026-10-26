# Zenoh SHM listener quiescence

## Classic and owned subscriber dispatch

Classic subscriber callbacks copy and release the native sample before handing
the message to a one-credit, creation-owned dispatch queue. One generic SDK worker
invokes the listener in order on an independent two-worker runtime. Delivery does
not create a task per message. Closing the registration wakes producers waiting
for credit before native subscriber retirement and worker drain.

A callback-originated publication reserves the matching copied-control queue
credits across all transports sharing its native session before entering the synchronous native
put. Saturation returns `ResourceExhausted` before publication. Reservations move
into the inline callback's queued message; cancelled/unused reservations release
their credit. No reservation lock is held across the native call and scoped
thread-local reservation state never crosses an await. This copied-control queue
does not establish a native SHM loan budget or change the prepared FrameSink path.
New dispatch targets wait for protected publications to finish before joining the
session's reservation set. They join before native declaration, so a callback
cannot appear midway through a put without its reserved credit. This wait holds
no registry lock. Callback-originated `ZenohRpcClient` queries use the same
reservation boundary before their synchronous native get; reply waiting is async.
Each RPC call uses one one-shot reply slot and disables native consolidation.
The native reply callback publishes its first result without blocking and discards
later duplicates; cancelling the waiter makes that send fail immediately. No
default reply FIFO or per-reply task sits between native delivery and the caller.
Classic registration reserves a pending map entry before native declaration.
Concurrent duplicates await that entry's result outside the registry lock;
cancelled creation closes its worker/credit and wakes those waiters. Removal
during creation closes admission and causes any late native declaration to retire
instead of becoming an active listener.
For RPC, that same pending entry owns both the subscriber and queryable. Setup
becomes ready only after both declarations succeed; cancellation or failure retires
both, and duplicate waiters observe the same setup result.
Owned encoded listeners use the same one-credit worker mechanism. A classic RPC
queryable shares its logical listener's subscriber worker, rather than spawning
another task for each query. A generation-scoped query ticket travels with the
copied message: failed enqueue, queue discard and cancellation of an entered
listener remove that exact pending query. A normally returned listener may still
delegate a response; its query remains until the response or request TTL expires.
One setup-owned task on the existing dispatch runtime retires expired queries even
when no subsequent traffic arrives. Query objects are dropped outside map locks.
Waiting for copied-query queue credit also ends at the request's original deadline;
it cannot strand a native get behind a full queue after that query has expired.
Removal closes query admission and clears its correlations before awaiting native
retirement, so cancelling unregister cannot leave its native queryable in the map.
Duplicate live request IDs do not replace the original query. A correlated reply
must reverse the original source and sink; an endpoint mismatch leaves the query
available for its valid response. A response with no live native query retains the
classic publish fallback (including late responses); clients still validate their
own outstanding request IDs and deadlines.
The owned core also reserves a pending entry before declaration. Repeated or
concurrent registration of the same listener/filters shares one subscriber and
setup result. Cancellation removes only that pending generation; removal during
setup retires a late subscriber. Owned pull receive uses a separate one-shot
result slot, not a default native FIFO: after its first valid copied frame, later
samples are ignored, and dropping the receive future retires the subscription.
The dynamic SHM control follows the same pending-registration ownership rule, while
delivery stays synchronous into its caller's FrameSink. Its pull operation also
has one first-result slot retaining that sample's native lease; absent attachments
are skipped and non-SHM payload errors are returned. Registry destruction closes
admission before dropping native handles, including when an entered callback
retains a separate dispatch reference. No registry lock crosses declaration or
quiescence waits.
Native sink registrations retain a setup-owned session/filter/thread identity.
Before publication, the binding refuses a put that would synchronously reenter an
already-entered local native sink, including through a different core sharing the
session or a nested callback. The identity lock is released before sink delivery;
publication holds no registry lock while invoking native code. Worker-based
handlers and replies targeting another sink/session keep their normal path.
This prevents recursive acquisition of Zenoh's mutable-callback lock without
adding a native mailbox or relying on a per-message TLS stack/allocation.

## Native SHM sinks

The dynamic allocating SHM control has an independent admission gate per sink.
`ZenohZeroCopyCoreBuilder::with_listener_quiesce_timeout` sets its positive,
representable deadline interval; default is five seconds. The classic
`UPTransportZenoh` registry has the separate copied-control lifecycle above.
Dynamic native callbacks enter the SDK's native admission guard before inspecting
or handing off the sample, so quiescence observes already-entered native pushes.
Zenoh can invoke local callbacks inline from a Tokio task. On that supported
multithread runtime, the synchronous native admission region uses `block_in_place`;
native threads outside Tokio enter it directly. This is separate from the async
SDK worker/application callback and does not create a task per message.

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
