# Native publisher readiness

`ZenohZeroCopyCoreBuilder::with_publisher_readiness(timeout)` optionally requires
an actual Zenoh matching subscriber before a native selected-wire frame is put.
The default remains publication without waiting. This setting belongs to the
binding: it uses Zenoh's publisher matching API, not an application sleep or a
logical uProtocol authority check.

One deadline covers declaration and repeated matching observations. No match returns
`DeadlineExceeded` without a put. A zero/unrepresentable timeout is rejected. An
in-flight wait owns the transmit buffer and temporary publisher; cancellation drops
those handles. No global publisher/ID registry is introduced. The matching publisher
is retained through the put, with the original priority, attachment and SHM payload.

Presence is a snapshot. A peer can leave after matching, so this is not a delivery
acknowledgement, remote application-processing proof, subscriber count or retransmit
policy. Wildcard subscribers match according to Zenoh's native key-expression rules.
The setting applies to initialized and uninitialized-loan send paths once the loan
becomes an initialized buffer; it does not alter payload identity or initialization.

Readiness, SHM pool sizing, callback quiescence and retained-sample lifetime remain
separate contracts. The native five-second unregister default is unchanged; a
readiness timeout never revokes another retained sample. Classic `UPTransportZenoh`
behavior is unchanged. Integration fixtures must still validate end-to-end delivery.
