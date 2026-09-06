# Multiplayer Overview

The built-in `renzora_network` implementation uses native UDP for connection lifecycle and reliable script events. It no longer uses Lightyear. Multiplayer remains an early, unauthenticated LAN/development feature—not an internet-ready service.

## What is available

- Dedicated server (`renzora --server`) and windowed host (`renzora --host`).
- Client connections through `action("net_connect", { address = "127.0.0.1", port = 7636 })`; disconnection through `action("net_disconnect")`.
- `rpc(name, args)` events and `on_rpc(name, args, from)`.
- Server-side `on_player_joined(id)` and `on_player_left(id)` hooks.
- Network status and metadata in the editor's network panels.

The server relays a client's RPC to every other client without self-echo. Only the server sees the originating client's ID; relayed clients receive `from = 0`. Metadata is not authority enforcement. Validate gameplay requests yourself.

Scripts can inspect status using `net_is_server()`, `net_is_client()`,
`net_is_connected()` and `net_player_count()`.

Automatic entity/Transform replication, interpolation, prediction and avatar spawning are **not implemented**. Older Lightyear documentation was stale; see [State Replication](replication.md). WebTransport/WebSocket configuration values do not enable those transports, and browser builds have no built-in UDP multiplayer.

## Bounded reliable delivery

Within a connection, accepted events are retried until acknowledged or the connection ends. Duplicates are suppressed, but events are **not ordered**.

- A peer may send at most 1,024 sequence positions beyond its oldest unacknowledged event. Newer acknowledgements cannot bypass a lost older event.
- Encoded datagrams are at most 4,096 bytes including headers. Retained encoded payload is at most 4 MiB per peer, plus bounded collection overhead.
- Receive history uses a fixed 128-byte bitmap and sequence floor. Old duplicates are acknowledged without delivery; unseen packets beyond the window are not acknowledged.
- Sequence numbers never wrap. Exhausting the 32-bit sequence space reports that a new connection is required.
- Each client/server poll consumes at most 1,024 datagrams. The server enforces `max_clients` (default 32).

Engine code can use `NetworkClient::try_send_event` for a `SendError`, or `NetworkServer::try_broadcast` for the list of refused recipients. `Backpressure` means retry later; `TooLarge` means reduce the message; `NotConnected` and `SequenceExhausted` require connection handling. Refusal consumes no sequence and never replaces accepted data.

Existing fire-and-forget methods and script RPCs log refused sends rather than queueing indefinitely. `rpc()` has no per-send acknowledgement callback. Applications needing guaranteed submission must use the checked API with their own bounded retry policy. A broadcast can succeed for some recipients and fail for others: use `NetworkServer::try_send_to` with the returned recipient addresses to retry only refused peers. Resending to everyone can create duplicate application events.

These bounds do not authenticate traffic or protect against stale packets across source-address reuse. There is no negotiated session token. Do not expose this transport to untrusted internet traffic.

## Configuration

See [Server Setup](server-setup.md) for role flags, project settings, binding behavior and connecting scripts. HTTP, marketplace, MCP and editor WebSocket services are separate from this game transport.
