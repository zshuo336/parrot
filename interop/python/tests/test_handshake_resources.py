"""Resource bounds belong to the network decoder, not the public TLV list API."""
import struct
import tracemalloc

import pytest
from parrot_protocol import wire


def tlv(tag, value):
    return struct.pack('<BH', tag, len(value)) + value


def hello(ack=False):
    # Independent wire fixture, including the legacy omitted chosen-codec ACK.
    return (tlv(1, b'peer') + tlv(4, b'\x01\0\0\0') +
            tlv(5, b'\0\0\x10\0') + tlv(6, b'\0') +
            (tlv(9, b'bin') if ack else b''))


def measured_decode(body, ack):
    # Build fixtures before tracing. Measure peak traced memory instead of wall
    # time; this catches retained entry lists and copied unknown payloads.
    tracemalloc.start()
    try:
        try:
            result = wire.decode_handshake(body, acknowledgement=ack)
        except ValueError as error:
            result = str(error)
        _, peak = tracemalloc.get_traced_memory()
    finally:
        tracemalloc.stop()
    assert peak < 64 * 1024, f'handshake decoder peak allocation: {peak} bytes'
    return result


@pytest.mark.parametrize('ack', [False, True])
@pytest.mark.parametrize('payload', [b'', b'future extension' * 4])
def test_unknown_flood_has_bounded_decoder_storage(ack, payload):
    body = hello(ack) + tlv(255, payload) * 32768
    body += b''.join(tlv(tag, b'\xff') for tag in [0, *range(10, 255)])
    assert measured_decode(body, ack) == wire.decode_handshake(hello(ack), acknowledgement=ack)


@pytest.mark.parametrize('tag', range(1, 10))
def test_known_duplicate_flood_has_bounded_decoder_storage(tag):
    body = hello(True) + tlv(tag, b'duplicate') * 32768
    assert measured_decode(body, True) == 'duplicate handshake field'


@pytest.mark.parametrize('ack', [False, True])
@pytest.mark.parametrize('tail', [b'\xff', b'\xff\x01', b'\xff\x02\0x', b'\x01\x02\0x'])
def test_truncated_tail_after_extensions_is_rejected_with_bounded_storage(ack, tail):
    body = hello(ack) + tlv(255, b'future') * 32768 + tail
    expected = 'truncated TLV header' if len(tail) < 3 else 'truncated TLV value'
    assert measured_decode(body, ack) == expected


def test_truncation_keeps_precedence_over_known_duplicate():
    body = hello() + tlv(1, b'duplicate') * 32768 + b'\xff'
    assert measured_decode(body, False) == 'truncated TLV header'


def test_generic_parser_still_returns_every_field_in_order_with_owned_bytes():
    entries = [(255, b'first'), (1, b'node'), (255, b'second'), (0, b'')]
    body = b''.join(tlv(tag, value) for tag, value in entries)
    assert wire.parse_tlv(body) == entries
    assert all(isinstance(value, bytes) for _, value in wire.parse_tlv(body))
    with pytest.raises(ValueError, match='truncated TLV'):
        wire.parse_tlv(body + b'\xff')
