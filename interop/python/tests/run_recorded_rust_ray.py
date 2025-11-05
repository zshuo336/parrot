"""Run the compiled Rust production sender against a real owned Ray deployment.

Usage: PYTHONPATH=interop/python python3 interop/python/tests/run_recorded_rust_ray.py
       /absolute/path/to/python_causal_context-test-binary /absolute/evidence/path
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import threading
import time

from parrot_protocol.ray_gw import ParrotDispatcher, RayAdminState, _serve_connection_guarded
import ray
from test_handler_recording_ray import recorded
from parrot_protocol.trace_recording import snapshot_otlp


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('binary', type=Path)
    parser.add_argument('evidence', type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.evidence.resolve()
    root.mkdir(parents=True)
    os.environ['PARROT_RECORDING_RAY_EVIDENCE'] = str(root / 'deployment')
    os.environ['PARROT_NODE_ID'] = 'ray-gw-1'

    class Paths:
        def mktemp(self, prefix):
            path = root / prefix
            path.mkdir()
            return path

    owner = recorded.__wrapped__(Paths())
    try:
        worker, _, _ = next(owner)
        state = RayAdminState()
        errors = []
        sockets = []
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0)); listener.listen(1)
            listener.settimeout(20)
            def serve():
                try:
                    connection, _ = listener.accept()
                    sockets.append(connection)
                    _serve_connection_guarded(connection, ParrotDispatcher(), worker, ray, state)
                except BaseException as error:
                    errors.append(repr(error))
            thread = threading.Thread(target=serve)
            thread.start()
            try:
                env = dict(os.environ, PARROT_CAUSAL_GATEWAY_PORT=str(listener.getsockname()[1]),
                           PARROT_CAUSAL_EXPECT_RAY='1', PARROT_CAUSAL_RUST_EVIDENCE=str(root / 'rust'))
                with (root / 'rust.log').open('xb') as log:
                    process = subprocess.Popen([str(binary), '--exact',
                        'rust_sender_observes_recorded_python_handler_children', '--nocapture'],
                        env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                    try:
                        code = process.wait(timeout=90)
                    except BaseException:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
                        raise
                assert code == 0, (root / 'rust.log').read_text()
            finally:
                thread.join(5)
                for connection in sockets:
                    if thread.is_alive():
                        try: connection.shutdown(socket.SHUT_RDWR)
                        except OSError: pass
                thread.join(20)
                assert not thread.is_alive() and not errors, errors
                state.management.close(time.monotonic() + 10)
                state.ask_admission.wait(time.monotonic() + 10)
            assert len(sockets) == 1
            snapshot = ray.get(worker.trace_snapshot.remote('/user/dispatcher'), timeout=20)
            records = snapshot['records']
            assert len(records) == 4 and snapshot['active'] == 0 and snapshot['dropped'] == 0
            assert [r['outcome'] for r in records] == ['ok', 'error', 'ok', 'ok']
            assert [r['parent_span_id'] for r in records] == ['04' * 8, '06' * 8, None, '08' * 8]
            assert [r['trace_id'] for r in records if r['parent_span_id']] == ['03' * 16, '05' * 16, '07' * 16]
            receipts = list((root / 'rust').glob('*/handler-receipts.json'))
            assert len(receipts) == 1
            observed = json.loads(receipts[0].read_text())
            for value, record in zip(observed, (records[0], records[2], records[3])):
                identity = bytes.fromhex(value['parent'])
                assert (identity[1:17].hex(), identity[17:].hex()) == (record['trace_id'], record['span_id'])
            assert len(observed) == 3
            (root / 'snapshot.json').write_text(json.dumps(snapshot, indent=2))
            (root / 'documents.json').write_text(json.dumps([snapshot_otlp(snapshot)], indent=2))
            assert ray.get(worker.trace_acknowledge.remote('/user/dispatcher', [r['span_id'] for r in records]), timeout=20) == 4
            after = ray.get(worker.trace_snapshot.remote('/user/dispatcher'), timeout=20)
            assert after['records'] == [] and after['acknowledged'] == 4
            (root / 'acknowledged.json').write_text(json.dumps(after, indent=2))
    finally:
        owner.close()
    (root / 'terminal.json').write_text(json.dumps(dict(status='passed', rust_exit=code, connections=len(sockets))))
    print('Rust -> TCP -> Ray worker -> deployment job/host: passed')


if __name__ == '__main__':
    main()
