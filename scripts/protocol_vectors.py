"""Read shared JSON fixtures; emit data-only Erlang terms for older OTP tests.

This helper does not implement Wire or admin encoding/decoding. Expected bytes
and independently stated fields stay in the JSON consumed by every dialect.
"""
import argparse
import json
from pathlib import Path
import struct
import sys


def require(ok, message):
    if not ok:
        raise ValueError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON key: ' + key)
        result[key] = value
    return result


def invalid_constant(value):
    raise ValueError('invalid JSON constant: ' + value)


def text(value, name, nonempty=False):
    require(isinstance(value, str) and (not nonempty or bool(value)), 'invalid ' + name)
    require('\0' not in value, 'NUL in ' + name)
    return value.encode('utf-8', errors='strict')


def uint(value, bits, name):
    require(type(value) is int and 0 <= value < 1 << bits, 'invalid ' + name)
    return value


def unhex(value, name):
    require(isinstance(value, str), 'invalid ' + name)
    result = bytes.fromhex(value)
    require(result.hex() == value.lower() and len(result) <= (1 << 24) + 4, 'invalid ' + name)
    return result


def load(kind, path):
    require(kind in ('wire', 'admin'), 'unknown fixture kind')
    value = json.loads(Path(path).read_text(encoding='utf-8'), object_pairs_hook=unique_object,
                       parse_constant=invalid_constant)
    require(isinstance(value, dict), 'fixture must be an object')
    require(value.get('format') == ('parrot-wire-1.0' if kind == 'wire' else 'parrot-admin-v2'), 'wrong fixture format')
    require(value.get('frozen') is True, 'fixture must be frozen')
    rows = value.get('vectors')
    require(isinstance(rows, list) and 0 < len(rows) <= 4096, 'invalid vector list')
    result, names = [], set()
    for row in rows:
        require(isinstance(row, dict), 'vector must be an object')
        name = text(row['name'], 'name', True)
        require(name not in names, 'duplicate vector name')
        names.add(name)
        item = dict(name=name, payload=unhex(row['payload_hex'], 'payload_hex'))
        if kind == 'wire':
            frame_type = row['frame_type']
            require(isinstance(frame_type, str) and len(frame_type) == 4 and
                    frame_type.startswith('0x') and
                    all(c in '0123456789abcdefABCDEF' for c in frame_type[2:]),
                    'invalid frame_type')
            item.update(ft=uint(int(frame_type, 16), 8, 'frame_type'),
                cid=uint(row['correlation_id'], 64, 'correlation_id'),
                flags=uint(row.get('flags', 0), 16, 'flags'),
                hop_count=uint(row.get('hop_count', 0), 8, 'hop_count'),
                hop_limit=uint(row.get('hop_limit', 8), 8, 'hop_limit'),
                sequence=uint(row.get('sequence', 0), 32, 'sequence'),
                path=text(row['path'], 'path'), key=text(row['type_key'], 'type_key'),
                bytes=unhex(row['bytes_hex'], 'bytes_hex'))
        result.append(item)
    return result


def external_term(value):
    """The data-only ETF subset: binary keys/values, unsigned integers, lists, maps."""
    if isinstance(value, str):
        value = value.encode('utf-8', errors='strict')
    if isinstance(value, bytes):
        return b'm' + struct.pack('>I', len(value)) + value
    if type(value) is int:
        require(0 <= value < 1 << 64, 'ETF integer outside fixture range')
        if value < 256:
            return bytes([97, value])
        digits = value.to_bytes((value.bit_length() + 7) // 8, 'little')
        return bytes([110, len(digits), 0]) + digits
    if isinstance(value, list):
        return b'l' + struct.pack('>I', len(value)) + b''.join(map(external_term, value)) + b'j'
    if isinstance(value, dict):
        return b't' + struct.pack('>I', len(value)) + b''.join(
            external_term(key) + external_term(item) for key, item in value.items())
    raise ValueError('unsupported fixture term')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('kind', choices=['wire', 'admin'])
    parser.add_argument('path', type=Path)
    options = parser.parse_args()
    try:
        rows = load(options.kind, options.path)
        sys.stdout.buffer.write(b'\x83' + external_term(rows))
    except (OSError, ValueError, UnicodeError, KeyError) as error:
        parser.exit(2, 'protocol vector fixture: ' + str(error) + '\n')


if __name__ == '__main__':
    main()
