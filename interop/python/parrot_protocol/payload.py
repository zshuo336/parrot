"""Strict application document batches: count, then (u64 id, u32 size, bytes).

Input and collection budgets are checked before materialization. Parsing a batch
has no application side effects; callers commit only after processing succeeds.
"""
import struct
from .wire import MAX_FRAME_LEN

MAX_DOCUMENTS = 65_536

def decode_documents(payload: bytes, *, text: bool = False):
    if len(payload) > MAX_FRAME_LEN or len(payload) < 4:
        raise ValueError("document batch byte budget or truncated count")
    count, = struct.unpack_from("<I", payload)
    if count > MAX_DOCUMENTS or count > (len(payload) - 4) // 12:
        raise ValueError("document count exceeds budget or available records")
    offset = 4
    documents = []
    for _ in range(count):
        if len(payload) - offset < 12:
            raise ValueError("truncated document header")
        docid, size = struct.unpack_from("<QI", payload, offset)
        offset += 12
        if size > len(payload) - offset:
            raise ValueError("truncated document body")
        body = payload[offset:offset + size]
        offset += size
        documents.append((docid, body.decode("utf-8") if text else body))
    if offset != len(payload):
        raise ValueError("trailing document batch bytes")
    return documents
