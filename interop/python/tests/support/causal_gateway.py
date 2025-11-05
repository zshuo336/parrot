"""Standard-library-only Python endpoint for the ordinary Rust test suite."""
import json
import os
from pathlib import Path
import socket
import sys
import time

from parrot_protocol.ray_gw import ParrotDispatcher, RayAdminState, _ComponentRuntime, _serve_connection_guarded
from parrot_protocol.trace import capture_parent
from parrot_protocol.trace_recording import TraceConfig, snapshot_otlp


def main():
    root = Path(sys.argv[1])
    dispatcher = ParrotDispatcher()

    @dispatcher.handler('bin:context')
    def context(key, payload):
        if payload == b'fail':
            raise ValueError('requested business failure')
        parent = capture_parent()
        return key, json.dumps(dict(parent=None if parent is None else parent.hex(), payload=payload.hex())).encode()

    state = RayAdminState()
    if os.environ.get('PARROT_CAUSAL_RECORDING') == '1':
        state.local_runtime = _ComponentRuntime(dispatcher, trace_config=TraceConfig())
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        listener.settimeout(15)
        (root / 'ready.tmp').write_text(json.dumps(dict(port=listener.getsockname()[1])))
        (root / 'ready.tmp').replace(root / 'ready.json')
        connection, _ = listener.accept()
        _serve_connection_guarded(connection, dispatcher, None, None, state)
    state.management.close(time.monotonic() + 10)
    state.ask_admission.wait(time.monotonic() + 10)
    if os.environ.get('PARROT_CAUSAL_RECORDING') == '1':
        snapshot = state.local_runtime.trace_snapshot()
        assert len(snapshot['records']) == 4 and snapshot['active'] == 0 and snapshot['dropped'] == 0
        assert [r['outcome'] for r in snapshot['records']] == ['ok', 'error', 'ok', 'ok']
        (root / 'snapshot.json').write_text(json.dumps(snapshot, indent=2))
        (root / 'documents.json').write_text(json.dumps([snapshot_otlp(snapshot)], indent=2))
    (root / 'retired.tmp').write_text(json.dumps(dict(ok=True)))
    (root / 'retired.tmp').replace(root / 'retired.json')


if __name__ == '__main__':
    main()
