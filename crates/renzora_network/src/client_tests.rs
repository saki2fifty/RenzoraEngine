use super::*;

fn fixture() -> (NetworkClient, UdpSocket) {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client = NetworkClient::connect(server.local_addr().unwrap(), 42).unwrap();
    (client, server)
}

fn event() -> GameEvent {
    GameEvent {
        name: "test".into(),
        data: vec![1],
    }
}

#[test]
fn real_socket_handshake_delivers_only_after_acceptance() {
    let (mut client, server) = fixture();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut buffer = [0; MAX_DATAGRAM];
    let (count, address) = server.recv_from(&mut buffer).unwrap();
    assert!(matches!(
        decode(&buffer[..count]),
        Some(Packet::ConnectRequest { client_id: 42 })
    ));
    server
        .send_to(&encode(&Packet::ConnectAccept { client_id: 99 }), address)
        .unwrap();
    server
        .send_to(
            &encode(&Packet::Reliable {
                seq: 0,
                event: event(),
            }),
            address,
        )
        .unwrap();
    server
        .send_to(&encode(&Packet::ConnectAccept { client_id: 42 }), address)
        .unwrap();
    server
        .send_to(
            &encode(&Packet::Reliable {
                seq: 0,
                event: event(),
            }),
            address,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut delivered = Vec::new();
    while delivered.is_empty() && Instant::now() < deadline {
        delivered.extend(client.update());
        std::thread::yield_now();
    }
    assert!(client.connected);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].name, "test");
    server
        .send_to(&encode(&Packet::Disconnect), address)
        .unwrap();
    while client.connected && Instant::now() < deadline {
        client.update();
        std::thread::yield_now();
    }
    assert!(client.closed);
}

#[test]
fn handshake_admits_only_matching_id_and_no_early_application_data() {
    let (mut client, _server) = fixture();
    let before = client.server.last_recv;
    for packet in [
        Packet::ConnectAccept { client_id: 99 },
        Packet::KeepAlive,
        Packet::Reliable {
            seq: 0,
            event: event(),
        },
        Packet::Ack { seq: 0 },
        Packet::Disconnect,
    ] {
        assert!(client.receive_packet(packet).is_none());
        assert!(!client.connected);
        assert_eq!(client.server.last_recv, before);
    }
    client.receive_packet(Packet::ConnectAccept { client_id: 42 });
    assert!(client.connected);
    assert_eq!(
        client
            .receive_packet(Packet::Reliable {
                seq: 0,
                event: event()
            })
            .unwrap()
            .name,
        "test"
    );
    assert!(client
        .receive_packet(Packet::Reliable {
            seq: 0,
            event: event()
        })
        .is_none());
}

#[test]
fn timeout_and_remote_disconnect_are_terminal_and_status_is_disconnected() {
    for established in [false, true] {
        let (mut client, _server) = fixture();
        client.connected = established;
        client.server.last_recv = Instant::now() - crate::transport::PEER_TIMEOUT;
        assert!(client.update().is_empty());
        assert!(!client.connected);
        assert!(client.closed);
        client.receive_packet(Packet::ConnectAccept { client_id: 42 });
        assert!(!client.connected);
        let mut app = App::new();
        app.insert_resource(client);
        app.insert_resource(NetworkStatus {
            state: ConnectionState::Connected,
            client_id: Some(42),
            ..default()
        });
        app.add_systems(Update, update_network_status);
        app.update();
        let status = app.world().resource::<NetworkStatus>();
        assert_eq!(status.state, ConnectionState::Disconnected);
        assert_eq!(status.client_id, None);
    }
    let (mut client, _server) = fixture();
    client.receive_packet(Packet::ConnectAccept { client_id: 42 });
    client.receive_packet(Packet::Disconnect);
    assert!(client.closed);
    assert!(client.update().is_empty());
    assert!(client.try_send_event(event()).is_err());
}

#[test]
fn malformed_and_foreign_datagrams_do_not_refresh_liveness() {
    let (mut client, server) = fixture();
    client.receive_packet(Packet::ConnectAccept { client_id: 42 });
    let before = client.server.last_recv;
    server
        .send_to(&[255; 8], client.socket.local_addr().unwrap())
        .unwrap();
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
    stranger
        .send_to(
            &encode(&Packet::Disconnect),
            client.socket.local_addr().unwrap(),
        )
        .unwrap();
    for _ in 0..20 {
        assert!(client.update().is_empty());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(client.connected);
    assert_eq!(client.server.last_recv, before);
    client.disconnect();
    assert!(!client.connected);
    assert!(client.closed);
}
