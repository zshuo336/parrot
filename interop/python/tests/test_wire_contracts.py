"""Independent Wire bytes test field preservation and handshake semantics."""
import struct

import pytest

from parrot_protocol import wire


def tlv(tag, value):
    return struct.pack('<BH', tag, len(value)) + value


def hello(node=b'peer', *, caps=3, extra=b''):
    return (tlv(1, node) + tlv(4, struct.pack('<I', caps)) +
            tlv(5, struct.pack('<I', 1 << 20)) + tlv(6, b'\0') + extra)


@pytest.mark.parametrize('sequence', [0, 1, 0x12345678, 0xffffffff])
def test_independent_frame_preserves_every_header_field(sequence):
    path, key, payload = '/目录'.encode(), 'bin:键'.encode(), b'\0\xffpayload'
    body = (struct.pack('<BBHQBBIH', 1, 0x13, 0x800a, 0xffffffffffffffff, 2, 255, sequence, 0) +
            struct.pack('<I', len(path)) + path + struct.pack('<I', len(key)) + key + payload)
    encoded = struct.pack('<I', len(body)) + body
    decoder = wire.FrameDecoder()
    decoder.feed(encoded)
    frame = decoder.next_frame()
    assert frame.sequence == sequence
    assert (frame.ft, frame.flags, frame.cid, frame.hop_count, frame.hop_limit) == (0x13, 0x800a, 0xffffffffffffffff, 2, 255)
    assert (frame.path, frame.type_key, frame.payload) == ('/目录', 'bin:键', payload)
    assert frame.encode() == encoded
    assert wire.build_frame(frame.ft, frame.cid, frame.path, frame.type_key, frame.payload,
                            frame.flags, frame.hop_count, frame.hop_limit, sequence=sequence) == encoded


@pytest.mark.parametrize('ft', [0, 5, 0x16, 0x80, 256, -1])
def test_encoder_rejects_unknown_frame_types(ft):
    with pytest.raises(ValueError):
        wire.build_frame(ft, 1, '', '', b'')


@pytest.mark.parametrize('sequence', [-1, 1 << 32, 1.5])
def test_encoder_rejects_invalid_sequence(sequence):
    with pytest.raises(ValueError):
        wire.build_frame(0x13, 1, '', '', b'', sequence=sequence)


@pytest.mark.parametrize('node', [b'', b'\0', b'left\0right', b'\xff'])
@pytest.mark.parametrize('ack', [False, True])
def test_decode_rejects_invalid_node_identity(node, ack):
    with pytest.raises(ValueError):
        wire.decode_handshake(hello(node), acknowledgement=ack)


@pytest.mark.parametrize('value', [b'bin', b'pb', b'wat', b''])
def test_hello_rejects_ack_only_tag(value):
    with pytest.raises(ValueError):
        wire.decode_handshake(hello(extra=tlv(9, value)))


@pytest.mark.parametrize('node', ['', '\0', 'left\0right', '\ud800', '界' * 21846])
@pytest.mark.parametrize('ack', [False, True])
def test_encoder_rejects_invalid_node_identity(node, ack):
    with pytest.raises(ValueError):
        (wire.handshake_ack_body if ack else wire.handshake_body)(node)


@pytest.mark.parametrize('caps,chosen', [(1, 'pb'), (2, 'bin'), (0, 'bin'), (3, 'wat'), (3, '')])
def test_ack_encoder_rejects_unshared_codec(caps, chosen):
    with pytest.raises(ValueError):
        wire.handshake_ack_body('peer', caps, chosen)


@pytest.mark.parametrize('caps,chosen', [(1, 'bin'), (2, 'pb'), (3, 'bin'), (0xffffffff, 'pb')])
def test_valid_ack_and_exact_utf8_limit(caps, chosen):
    node = '界' * 21845  # exactly 65535 bytes, not 65535 characters
    body = wire.handshake_ack_body(node, caps, chosen)
    assert body[:3] == b'\x01\xff\xff'
    parsed = wire.decode_handshake(body, acknowledgement=True)
    assert parsed['node_id'] == node and parsed['chosen_codec'] == chosen


def test_unknown_tags_and_legacy_ack_remain_supported():
    body = hello(extra=tlv(99, b'\xff') + tlv(99, b'\0'))
    assert wire.decode_handshake(body)['node_id'] == 'peer'
    assert wire.decode_handshake(body, acknowledgement=True)['chosen_codec'] == 'bin'
    with pytest.raises(ValueError):
        wire.decode_handshake(hello(caps=2), acknowledgement=True)
    assert wire.decode_handshake(hello(caps=2, extra=tlv(9, b'pb')), acknowledgement=True)['chosen_codec'] == 'pb'


@pytest.mark.parametrize('field,value', [('flags', -1), ('flags', 65536), ('cid', -1), ('cid', 1 << 64),
                                        ('hop_count', -1), ('hop_count', 256), ('hop_limit', 0),
                                        ('hop_limit', 256), ('flags', 1.5), ('cid', 1.5)])
def test_header_scalars_fail_before_encoding(field, value):
    fields = dict(ft=0x13, cid=1, path='', type_key='', payload=b'')
    fields[field] = value
    with pytest.raises(ValueError):
        wire.build_frame(**fields)


@pytest.mark.parametrize('caps', [-1, 1 << 32, 1.5])
@pytest.mark.parametrize('ack', [False, True])
def test_capability_width_is_validated_before_encoding(caps, ack):
    with pytest.raises(ValueError):
        (wire.handshake_ack_body if ack else wire.handshake_body)('peer', caps)


def test_maximum_unsigned_capabilities_and_hello_without_codecs_can_be_encoded():
    # HELLO declares capabilities; ACK must choose a shared codec. Encoding a
    # declaration does not prove the current local peer has a codec in common.
    assert wire.parse_tlv(wire.handshake_body('peer', 0))[1] == (4, b'\0' * 4)
    assert wire.decode_handshake(wire.handshake_body('peer', 0xffffffff))['capabilities'] == 0xffffffff
