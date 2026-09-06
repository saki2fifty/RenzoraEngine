# Server Setup

The game/runtime executable supports a headless dedicated server or a windowed host. The editor is a separate executable, not a DLL discovered beside the runtime.

```bash
renzora --server --port 7636 --tick-rate 64 --max-clients 32
renzora --host --port 7636
```

`--host` wins if both role flags are present. A host runs the server in its windowed game world; it does not create a Lightyear in-process client. A dedicated server uses the headless loop and can run server-side scripts. Exported runtime builds must include networking.

## Configuration

CLI overrides take precedence over `[network]` in `project.toml`, then built-in defaults:

```toml
[network]
server_addr = "127.0.0.1"
port = 7636
transport = "udp"
tick_rate = 64
max_clients = 32
```

`--port`, `--tick-rate` and `--max-clients` select the listening port, headless simulation frequency and admission cap. `server_addr` selects the local listening address; `--addr`/`--address` overrides it. The default `127.0.0.1` accepts only local connections. Set `0.0.0.0` explicitly to listen on all IPv4 interfaces, and use firewall rules to restrict access. A bind failure leaves the server disconnected instead of silently switching to another interface. This address restriction does not add connection authentication or encryption.

The client cap is enforced. Existing clients can retry their handshake while full; new clients wait for a slot. Zero capacity admits none. Silent peers expire after ten seconds. Native UDP is the only built-in transport; browser builds cannot host or join.

## Connecting

A client starts disconnected:

```lua
function on_ready()
    action("net_connect", { address = "127.0.0.1", port = 7636 })
end
-- Later: action("net_disconnect")
```

Use `rpc(name, args)` and `on_rpc(name, args, from)` for events. Join/leave hooks are server-side. The server relays client events to other clients without echoing them to their sender; this is not authority validation.

## Limits and security

There is no authentication, encryption or session token. Use trusted LAN/development environments only. Automatic Transform replication, prediction, avatars and alternate transports are absent.

Send windows, packet sizes and per-poll work are bounded. Refused sends are reported, not queued indefinitely; see [Multiplayer Overview](overview.md). Admission limits do not make this an internet-safe protocol.
