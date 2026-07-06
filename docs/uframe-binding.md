# Zenoh UFrame Binding

Binding id: `zenoh.uframe.metadata-attachment.v1`

## Physical Placement

Selected-wire UFrame metadata is carried in the Zenoh attachment bytes. The attachment contains the up-rust native-prefix metadata envelope: magic/version, selected-wire identity, payload-family identity, metadata-layout identity, and the selected metadata profile bytes.

The Zenoh payload remains the application payload bytes for the selected wire. The metadata attachment is not part of the application payload.

## Metadata Profiles

The default profile is the canonical UFrame field-block metadata profile identified by `org.eclipse.uprotocol.metadata.uframe-fields`.

The legacy protobuf-`UAttributes` metadata profile remains compatibility-only and must be selected explicitly by a legacy-named API. Mixed-profile decode is rejected as an unknown metadata layout before a frame is exposed to users.

## Routing Mirror Validation

Zenoh key expressions continue to mirror the source and optional sink filters used by the transport. Received selected-wire frames are decoded from the attachment and then checked against the requested source and sink filters using semantic UFrame metadata accessors.

## Malformed Input

Missing, malformed, wrong-wire, wrong-payload-family, wrong-profile, and filter-mismatched attachments are rejected before public selected-wire frame exposure. Listener paths drop rejected frames instead of dispatching them.
