"""Negotiated limits start after ACK and cover prefixes already in the input buffer."""
import struct

import pytest
from parrot_protocol import wire
from test_gateway_envelopes import connection
from test_wire_contracts import tlv


def hello(limit, ack=False, extended=False):
    body = (tlv(1, b'peer') + tlv(4, b'\x01\0\0\0') + tlv(5, struct.pack('<I', limit)) +
            tlv(6, b'\0') + (tlv(9, b'bin') if ack else b''))
    if extended:
        body += tlv(255, b'x' * 40) * 32768  # >1MiB, < global 16MiB
    return body


@pytest.mark.parametrize('initiator', [False, True])
@pytest.mark.parametrize('limit,extended', [(28, False), (256, False), (2 << 20, True)])
def test_tcp_limits_activate_after_handshake_and_reject_oversize_prefix(monkeypatch, initiator, limit, extended):
    with connection(monkeypatch, initiator=initiator, tcp=True) as (peer, calls, receive, _):
        if initiator:
            assert receive().ft == wire.FT_HANDSHAKE
        kind = wire.FT_HANDSHAKE_ACK if initiator else wire.FT_HANDSHAKE
        peer.sendall(wire.build_frame(kind, 1, '', '', hello(limit, initiator, extended)) +
                     wire.build_frame(wire.FT_HEARTBEAT, 2, '', '', b''))
        if not initiator:
            assert receive().ft == wire.FT_HANDSHAKE_ACK
        assert receive().ft == wire.FT_HEARTBEAT_ACK
        negotiated = min(limit, 1 << 20)
        # Exactly-bound TELL reaches the production driver; heartbeat is an
        # ordering barrier for this synchronous worker spy.
        peer.sendall(wire.build_frame(wire.FT_TELL, 0, '', '', b'x' * (negotiated - 28)) +
                     wire.build_frame(wire.FT_HEARTBEAT, 3, '', '', b''))
        assert receive().ft == wire.FT_HEARTBEAT_ACK
        assert calls == [('tell', '', b'x' * (negotiated - 28))]
        peer.sendall(struct.pack('<I', negotiated + 1))
        assert receive() is None, 'must reject from length alone without waiting for body'


@pytest.mark.parametrize('initiator', [False, True])
def test_pipelined_oversize_prefix_cannot_use_pre_handshake_limit(monkeypatch, initiator):
    with connection(monkeypatch, initiator=initiator, tcp=True) as (peer, calls, receive, _):
        if initiator:
            assert receive().ft == wire.FT_HANDSHAKE
        kind = wire.FT_HANDSHAKE_ACK if initiator else wire.FT_HANDSHAKE
        peer.sendall(wire.build_frame(kind, 1, '', '', hello(256, initiator)) + struct.pack('<I', 257))
        if not initiator:
            assert receive().ft == wire.FT_HANDSHAKE_ACK
        assert receive() is None
        assert calls == []


@pytest.mark.parametrize('initiator', [False, True])
@pytest.mark.parametrize('peer_limit', [128, 2 << 20])
def test_outbound_reply_uses_minimum_of_local_and_peer_advertisements(monkeypatch, initiator, peer_limit):
    negotiated = min(peer_limit, 1 << 20)
    with connection(monkeypatch, initiator=initiator, tcp=True,
                    reply_payload=b'x' * negotiated) as (peer, calls, receive, _):
        if initiator:
            assert receive().ft == wire.FT_HANDSHAKE
        kind = wire.FT_HANDSHAKE_ACK if initiator else wire.FT_HANDSHAKE
        peer.sendall(wire.build_frame(kind, 1, '', '', hello(peer_limit, initiator)) +
                     wire.build_frame(wire.FT_ASK, 9, '/u', 'bin:test', b'\0' * 4))
        if not initiator:
            assert receive().ft == wire.FT_HANDSHAKE_ACK
        assert receive() is None, 'undeliverable reply must retire the connection'
        assert calls == [('ask', 'bin:test', b'')]
