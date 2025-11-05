"""One shared wire corpus: malformed input is never a legitimate partial frame."""
import pathlib

import pytest

from parrot_protocol.wire import FrameDecoder

CORPUS = pathlib.Path(__file__).resolve().parents[3] / "docs/vectors/wire1-boundaries.tsv"
CASES = [line.split("\t") for line in CORPUS.read_text().splitlines() if not line.startswith("#")]


@pytest.mark.parametrize("name,expected,encoded", CASES, ids=[c[0] for c in CASES])
def test_shared_boundary(name, expected, encoded):
    decoder = FrameDecoder()
    decoder.feed(bytes.fromhex(encoded))
    if expected == "reject":
        with pytest.raises(ValueError):
            decoder.next_frame()
    else:
        frame = decoder.next_frame()
        assert (frame is not None) == (expected == "frame"), name
        if frame is not None:
            assert frame.cid == 0x123456789ABCDEF0
            assert frame.encode() == bytes.fromhex(encoded)


def test_every_valid_split_and_pipelined_tail():
    for _, expected, encoded in CASES:
        if expected != "frame":
            continue
        wire = bytes.fromhex(encoded)
        for split in range(len(wire)):
            decoder = FrameDecoder()
            decoder.feed(wire[:split])
            assert decoder.next_frame() is None
            decoder.feed(wire[split:] + wire)
            assert decoder.next_frame().encode() == wire
            assert decoder.next_frame().encode() == wire
            assert decoder.next_frame() is None
