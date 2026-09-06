# State Replication

Automatic scene/component replication is **not implemented** in the current built-in UDP transport. Adding `Networked` does not send an entity or its Transform to another machine.

Earlier documentation described a removed Lightyear implementation. The current `renzora_network/src/protocol.rs` does not register component channels; the wire protocol carries connection messages and reliable `GameEvent` RPCs only.

## Existing components

`Networked`, `NetworkId`, `NetworkPlayer`, `NetworkOwner` and `NetworkTransform` remain scene metadata and integration points. The server assigns IDs to marked entities, but this is local bookkeeping—not transmission. `NetworkTransform` settings do not enable interpolation, rotation or scale synchronization.

Prediction, reconciliation, automatic avatar spawning, authority enforcement and mesh replication are also absent. The prediction module is a placeholder.

## What works today

Scripts exchange events using `rpc(name, args)` and receive them through `on_rpc(name, args, from)`. Server-side join/leave hooks report connection lifecycle. Game-specific state synchronization can be built on these events, but the engine does not automatically map received state onto scene entities.

The reliable channel suppresses duplicates within a connection but is **not ordered**. It has bounded windows and reports rejected sends; see [Multiplayer Overview](overview.md). A relayed client's original ID is not preserved at other clients (`from` is zero there).

Native UDP is for trusted local development. Authentication, encryption and session-token protection are not provided.
