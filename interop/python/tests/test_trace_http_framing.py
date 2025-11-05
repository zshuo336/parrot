"""A JSON prefix is not a successful receipt when HTTP framing is incomplete."""
from contextlib import contextmanager
import socket
import threading
import unittest

from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder


@contextmanager
def raw_collector(response):
    listener=socket.socket()
    listener.bind(('127.0.0.1',0))
    listener.listen(1)
    listener.settimeout(0.1)
    received=[]
    errors=[]
    stopping=threading.Event()
    responses=response if isinstance(response,list) else [response]
    def serve():
        try:
            while len(received)<len(responses) and not stopping.is_set():
                try:connection,_=listener.accept()
                except socket.timeout:continue
                with connection:
                    connection.settimeout(5)
                    with connection.makefile('rb') as stream:
                        request=stream.readline()
                        headers={}
                        while True:
                            line=stream.readline()
                            if line==b'\r\n':break
                            if not line:raise RuntimeError('incomplete request headers')
                            name,value=line.split(b':',1)
                            headers[name.lower()]=value.strip()
                        body=stream.read(int(headers[b'content-length']))
                        received.append((request,body))
                    try:
                        connection.sendall(responses[len(received)-1])
                    except (BrokenPipeError,ConnectionResetError):
                        # An exporter may reject invalid headers before reading
                        # the body. Positive cases still require its receipt.
                        pass
        except BaseException as error:
            errors.append(error)
    thread=threading.Thread(target=serve)
    thread.start()
    try:
        yield 'http://127.0.0.1:'+str(listener.getsockname()[1])+'/v1/traces',received
    finally:
        stopping.set()
        thread.join(6)
        listener.close()
        if thread.is_alive():raise RuntimeError('collector thread did not retire')
        if errors:raise RuntimeError('collector failed') from errors[0]


class TraceHttpFramingTests(unittest.TestCase):
    def exchange(self,headers,body):
        response=(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n'
                  b'Connection: close\r\n'+headers+b'\r\n'+body)
        with raw_collector(response) as (endpoint,received):
            recorder=TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=1,max_attempts=1,interval=0.01)))
            with recorder.handler('/user/orders','complete','ask'):
                pass
            result=recorder.finish_export(3)
            self.assertEqual(len(received),1)
            self.assertFalse(result['running'])
            return result,recorder.snapshot()

    def test_truncated_content_length_must_not_release_records(self):
        response=(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n'
                  b'Content-Length: 100\r\nConnection: close\r\n\r\n{}')
        with raw_collector(response) as (endpoint,received):
            recorder=TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=1,max_attempts=1,interval=0.01)))
            with recorder.handler('/user/orders','complete','ask'):
                pass
            result=recorder.finish_export(3)
            self.assertEqual(len(received),1)
            self.assertFalse(result['running'])
            self.assertFalse(result['ok'],'truncated HTTP receipt was treated as successful export')
            self.assertEqual(result['pending'],1)
            self.assertEqual(result['exported'],0)
            self.assertEqual(recorder.snapshot()['acknowledged'],0)

    def test_incomplete_or_ambiguous_http_framing_retains_records(self):
        cases=[
            ('truncated-length',b'Content-Length: 100\r\n',b'{}'),
            ('negative-length',b'Content-Length: -1\r\n',b'{}'),
            ('invalid-length',b'Content-Length: invalid\r\n',b'{}'),
            ('conflicting-lengths',b'Content-Length: 2\r\nContent-Length: 100\r\n',b'{}'),
            ('repeated-length',b'Content-Length: 2\r\nContent-Length: 2\r\n',b'{}'),
            ('conflicting-framing',b'Transfer-Encoding: chunked\r\nContent-Length: 2\r\n',b'2\r\n{}\r\n0\r\n\r\n'),
            ('unknown-transfer-coding',b'Transfer-Encoding: gzip\r\n',b'{}'),
            ('mislabelled-content-encoding',b'Content-Encoding: gzip\r\nContent-Length: 2\r\n',b'{}'),
            ('repeated-transfer-coding',b'Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n',b'2\r\n{}\r\n0\r\n\r\n'),
            ('truncated-chunk',b'Transfer-Encoding: chunked\r\n',b'4\r\n{}'),
            ('wrong-chunk-terminator',b'Transfer-Encoding: chunked\r\n',b'2\r\n{}xx0\r\n\r\n'),
            ('signed-chunk-size',b'Transfer-Encoding: chunked\r\n',b'+2\r\n{}\r\n0\r\n\r\n'),
            ('hex-prefix-chunk-size',b'Transfer-Encoding: chunked\r\n',b'0x2\r\n{}\r\n0\r\n\r\n'),
            ('missing-terminal-chunk',b'Transfer-Encoding: chunked\r\n',b'2\r\n{}\r\n'),
            ('missing-trailer-terminator',b'Transfer-Encoding: chunked\r\n',b'2\r\n{}\r\n0\r\n'),
            ('excessive-trailers',b'Transfer-Encoding: chunked\r\n',
             b'2\r\n{}\r\n0\r\n'+b'X-Info: x\r\n'*101+b'\r\n'),
            ('oversized-trailer-line',b'Transfer-Encoding: chunked\r\n',
             b'2\r\n{}\r\n0\r\nX-Info: '+b'x'*8192+b'\r\n\r\n'),
            ('oversized-framing-total',b'Transfer-Encoding: chunked\r\n',
             b'2\r\n{}\r\n0\r\n'+(b'X-Info: '+b'x'*700+b'\r\n')*100+b'\r\n'),
        ]
        for name,headers,body in cases:
            with self.subTest(case=name):
                result,snapshot=self.exchange(headers,body)
                self.assertFalse(result['ok'],name)
                self.assertEqual(result['pending'],1)
                self.assertEqual(result['exported'],0)
                self.assertEqual(snapshot['acknowledged'],0)

    def test_complete_fixed_chunked_and_close_delimited_receipts_are_accepted(self):
        for name,headers,body in [
            ('fixed',b'Content-Length: 2\r\n',b'{}'),
            ('explicit-identity',b'Content-Encoding: identity\r\nContent-Length: 2\r\n',b'{}'),
            ('leading-zero-length',b'Content-Length: 0002\r\n',b'{}'),
            ('chunked',b'Transfer-Encoding: chunked\r\n',b'2\r\n{}\r\n0\r\n\r\n'),
            ('split-chunks',b'Transfer-Encoding: chunked\r\n',b'1\r\n{\r\n1\r\n}\r\n0\r\n\r\n'),
            ('chunked-trailer',b'Transfer-Encoding: chunked\r\n',b'2;name=value\r\n{}\r\n0\r\nX-Info: value\r\n\r\n'),
            ('maximum-trailer-count',b'Transfer-Encoding: chunked\r\n',b'2\r\n{}\r\n0\r\n'+b'X-Info: x\r\n'*100+b'\r\n'),
            ('close-delimited',b'',b'{}'),
        ]:
            with self.subTest(case=name):
                result,snapshot=self.exchange(headers,body)
                self.assertTrue(result['ok'],result)
                self.assertEqual(result['pending'],0)
                self.assertEqual(result['exported'],1)
                self.assertEqual(snapshot['acknowledged'],1)

    def test_response_byte_bound_accepts_exact_boundary_and_rejects_excess(self):
        for framing in ['fixed','chunked','close']:
            for length in [65536,65537]:
                with self.subTest(framing=framing,length=length):
                    body=b'{}'+b' '*(length-2)
                    if framing=='fixed':
                        headers=b'Content-Length: '+str(length).encode()+b'\r\n'
                    elif framing=='chunked':
                        headers=b'Transfer-Encoding: chunked\r\n'
                        body=format(length,'x').encode()+b'\r\n'+body+b'\r\n0\r\n\r\n'
                    else:headers=b''
                    result,snapshot=self.exchange(headers,body)
                    self.assertEqual(result['ok'],length==65536)
                    self.assertEqual(snapshot['acknowledged'],int(length==65536))

    def test_explicit_retry_after_truncated_response_keeps_same_span_identity(self):
        prefix=b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n'
        with raw_collector([prefix+b'Content-Length: 100\r\n\r\n{}',
                            prefix+b'Content-Length: 2\r\n\r\n{}']) as (endpoint,received):
            recorder=TraceRecorder(TraceConfig(export=TraceExportConfig(endpoint,
                request_timeout=1,max_attempts=1,interval=0.01)))
            with recorder.handler('/user/orders','complete','ask'):
                pass
            first=recorder.finish_export(3)
            self.assertFalse(first['ok'])
            self.assertEqual(first['pending'],1)
            second=recorder.finish_export(3)
            self.assertTrue(second['ok'],second)
            self.assertEqual(second['exported'],1)
            self.assertEqual(second['attempts'],2)
            self.assertEqual(len(received),2)
            self.assertEqual(received[0][1],received[1][1])


if __name__=='__main__':unittest.main(verbosity=2)
