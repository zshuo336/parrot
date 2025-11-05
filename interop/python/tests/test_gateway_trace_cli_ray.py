"""Actual CLI options must reach a collector before successful signal exit."""
import json
import os
from pathlib import Path
import re
import signal
import socket
import struct
import subprocess
import sys
import threading

import pytest

from parrot_protocol import wire
from test_causal_context import PARENT, OTHER, ask
from test_trace_export import collector, wait_for


@pytest.mark.parametrize('transport',['listen','register'])
def test_actual_module_cli_exports_before_exit_in_both_network_modes(tmp_path,transport):
    configured=os.environ.get('PARROT_CLI_TRACE_EVIDENCE')
    root=(Path(configured)/transport) if configured else tmp_path
    root.mkdir(parents=True,exist_ok=True)
    fixture=Path(__file__).with_name('support')/'cli_trace_gateway.py'
    log_path=root/'gateway.log'
    with socket.socket() as listener,collector() as sink,log_path.open('xb') as log:
        sink.block=threading.Event()
        listener.bind(('127.0.0.1',0));listener.listen(1);listener.settimeout(30)
        argv=['0']
        if transport=='register':argv+=['parrot=127.0.0.1:'+str(listener.getsockname()[1])]
        argv+=['--trace-export-endpoint',sink.endpoint,'--trace-service-name','cli-test-service',
               '--trace-capacity','4','--trace-batch-size','1','--trace-interval','0.01']
        python=[sys.executable]+(['-O'] if sys.flags.optimize else [])
        process=subprocess.Popen(python+[str(fixture),str(root)]+argv,
            env=dict(os.environ,PYTHONUNBUFFERED='1'),stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        try:
            if transport=='listen':
                def ready():
                    if process.poll() is not None:raise AssertionError(log_path.read_text())
                    return re.search(r'RAY_GW_PORT=([0-9]+)',log_path.read_text())
                wait_for(ready,timeout=30)
                peer=socket.create_connection(('127.0.0.1',int(ready()[1])),timeout=10)
            else:
                peer,_=listener.accept()
                peer.settimeout(10)
            with peer:
                decoder=wire.FrameDecoder()
                def receive():
                    while True:
                        frame=decoder.next_frame()
                        if frame is not None:return frame
                        data=peer.recv(65536)
                        assert data
                        decoder.feed(data)
                caps=3|wire.CAPS_TRACE_CONTEXT
                if transport=='listen':
                    peer.sendall(wire.build_frame(wire.FT_HANDSHAKE,1,'','',wire.handshake_body('cli-client',caps)))
                    assert receive().ft==wire.FT_HANDSHAKE_ACK
                else:
                    hello=receive();assert hello.ft==wire.FT_HANDSHAKE
                    wire.decode_handshake(hello.payload)
                    peer.sendall(wire.build_frame(wire.FT_HANDSHAKE_ACK,hello.cid,'','',
                                                 wire.handshake_ack_body('cli-client',caps)))
                for cid,payload,parent in [(2,struct.pack('<Q',40),PARENT),(3,b'invalid',OTHER)]:
                    frame=ask(cid,payload,parent);frame.path='/user/probe';frame.type_key='bin:u:Ping'
                    peer.sendall(frame.encode());reply=receive()
                    assert reply.cid==cid
                    if cid==2:assert reply.ft==wire.FT_REPLY and reply.payload==struct.pack('<Q',42)
                    else:assert reply.ft==wire.FT_REPLY_ERR
                assert sink.received.wait(5)
                process.send_signal(signal.SIGTERM)
                # A received HTTP request is not an acknowledgement. The CLI
                # must retain its owner until the collector releases its reply.
                with pytest.raises(subprocess.TimeoutExpired):process.wait(timeout=0.15)
                sink.block.set()
                assert process.wait(timeout=30)==0,log_path.read_text()
            assert (root/'finished.txt').read_text()=='retired\n'
            retirement=json.loads((root/'cluster/cluster-retirement.json').read_text())
            assert retirement['ok'] and retirement['observed'] and not retirement['errors']
            records=sink.spans()
            assert len(records)==2 and [s['status']['code'] for s in records]==[1,2]
            for record,parent in zip(records,[PARENT,OTHER]):
                assert record['traceId']==parent.trace_id.hex()
                assert record['parentSpanId']==parent.span_id.hex() and record['spanId']!=parent.span_id.hex()
            for document in sink.documents():
                for resource in document['resourceSpans']:
                    assert dict((a['key'],a['value']['stringValue']) for a in resource['resource']['attributes'])['service.name']=='cli-test-service'
            (root/'documents.json').write_text(json.dumps(sink.documents(),indent=2))
            (root/'receipt.json').write_text(json.dumps(dict(transport=transport,cli=True,exit=0,
                collector_release_required=True,retired=True,spans=2),indent=2))
        finally:
            sink.block.set()
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=10)
                    (root/'forced-kill.json').write_text(json.dumps(dict(
                        timeout=True,process_group=process.pid,retirement_confirmed=False)))
