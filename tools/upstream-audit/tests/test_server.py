import base64
import concurrent.futures
import gzip
import hashlib
import http.client
import importlib.util
import json
import pathlib
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

spec = importlib.util.spec_from_file_location('audit', pathlib.Path(__file__).parents[1] / 'server.py')
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class Fixture(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    received = []
    release = threading.Event()
    def log_message(self, *args):
        pass
    def do_GET(self):
        self.fixture()
    def do_POST(self):
        self.fixture()
    def reply(self, status, body, headers=(), content_type='application/json'):
        self.send_response_only(status)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Content-Type', content_type)
        for k,v in headers:
            self.send_header(k,v)
        self.end_headers()
        self.wfile.write(body)
    def chunk(self, body):
        self.wfile.write(('%x\r\n' % len(body)).encode() + body + b'\r\n')
        self.wfile.flush()
    def fixture(self):
        body = self.rfile.read(int(self.headers.get('Content-Length','0')))
        self.received.append((self.command,self.path,dict(self.headers),body))
        if self.path.endswith('/models'):
            self.reply(200,b'{"data":[{"id":"fixture-model"}],"unknown":42}')
        elif self.path.endswith('/error'):
            self.reply(429,b'{"error":{"type":"fixture","unknown":[1,2]}}',[('Retry-After','7')])
        elif self.path.endswith('/redirect'):
            self.reply(307,b'unchanged',[('Location','http://127.0.0.1:1/private')])
        elif self.path.endswith('/stream'):
            self.send_response_only(200)
            self.send_header('Content-Type','text/event-stream')
            self.send_header('Transfer-Encoding','chunked')
            self.end_headers()
            self.chunk(b'event: message_start\ndata: {"type":"message_start","message":{"model":"fixture-model","unknown":42}}\n\n')
            self.release.wait(5)
            self.chunk(b'data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}\n\ndata: {"type":"message_stop"}\n\n')
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        elif self.path.endswith('/gzip'):
            data=gzip.compress(b'data: {"type":"message_start","message":{"model":"gzip-fixture"}}\n\n')
            self.reply(200,data,[('Content-Encoding','gzip')],'text/event-stream')
        elif self.path.endswith('/conflicting-framing'):
            self.send_response_only(200)
            self.send_header('Content-Type','application/json')
            self.send_header('Transfer-Encoding','chunked')
            self.send_header('Content-Length','900')
            self.end_headers()
            self.chunk(b'{"unknown":42}')
            self.wfile.write(b'0\r\n\r\n')
            self.wfile.flush()
        else:
            self.reply(200,body,[('Set-Cookie','private=value'),('X-Unknown-Upstream','unchanged')])


class Tests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.upstream=ThreadingHTTPServer(('127.0.0.1',0),Fixture)
        cls.upstream.daemon_threads=True
        threading.Thread(target=cls.upstream.serve_forever,daemon=True).start()
    @classmethod
    def tearDownClass(cls):
        Fixture.release.set()
        cls.upstream.shutdown()
        cls.upstream.server_close()
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.app=audit.AuditApp(self.temp.name,'https://example.test/relay-audit',allow_private=True)
        self.base='http://127.0.0.1:'+str(self.upstream.server_port)+'/prefix/v1'
        self.app.config.update({'upstreamBaseUrl':self.base,'upstreamApiKey':'upstream-secret','enabled':True})
        self.state=self.app.config.snapshot()
        self.server=audit.AuditHTTPServer(('127.0.0.1',0),self.app)
        threading.Thread(target=self.server.serve_forever,daemon=True).start()
        Fixture.release.clear()
        Fixture.received.clear()
    def tearDown(self):
        Fixture.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.temp.cleanup()
    def request(self,method,path,body=None,admin=False,headers=None):
        h={'x-audit-admin-key':self.state['admin_key']} if admin else {'x-api-key':self.state['relay_key']}
        h.update(headers or {})
        if isinstance(body,dict):
            h['Content-Type']='application/json'
            body=json.dumps(body).encode()
        c=http.client.HTTPConnection('127.0.0.1',self.server.server_port,timeout=10)
        c.request(method,path,body,h)
        r=c.getresponse()
        result=r.status,r.read(),dict(r.getheaders())
        c.close()
        return result
    def detail(self):
        for _ in range(100):
            records=self.app.records.list()['records']
            if records and records[0]['state']!='running':
                return self.app.records.detail(records[0]['id'])
            time.sleep(.01)
        self.fail('capture did not settle')
    def test_raw_payload_headers_and_secret_redaction(self):
        payload=b'{ "model": "fixture", "unknown": [null,1234567890123456789], "stream": false }'
        status,body,h=self.request('POST','/v1/messages',payload,headers={'Cookie':'client=secret','X-Extra':'yes','Connection':'X-Remove','X-Remove':'gone','Content-Type':'application/json'})
        self.assertEqual((status,body),(200,payload))
        method,path,forwarded,raw=Fixture.received[-1]
        self.assertEqual(path,'/prefix/v1/messages')
        self.assertEqual(raw,payload)
        lowered={k.lower():v for k,v in forwarded.items()}
        self.assertEqual(lowered['x-api-key'],'upstream-secret')
        self.assertEqual(lowered['x-extra'],'yes')
        self.assertNotIn('cookie',lowered)
        self.assertNotIn('x-remove',lowered)
        detail=self.detail()
        self.assertEqual(base64.b64decode(detail['request']['bodyBase64']),payload)
        self.assertEqual(detail['request']['sha256'],hashlib.sha256(payload).hexdigest())
        self.assertEqual(detail['request']['headers']['x-api-key'],'[redacted]')
        self.assertEqual(detail['request']['headers']['Cookie'],'[redacted]')
        self.assertEqual(detail['response']['headers']['Set-Cookie'],'[redacted]')
    def test_sse_first_event_before_completion(self):
        c=http.client.HTTPConnection('127.0.0.1',self.server.server_port,timeout=5)
        c.request('POST','/v1/stream',b'{}',{'x-api-key':self.state['relay_key']})
        r=c.getresponse()
        first=r.read1(65536)
        self.assertIn(b'message_start',first)
        self.assertFalse(Fixture.release.is_set())
        Fixture.release.set()
        rest=r.read()
        c.close()
        self.assertIn(b'opaque-signature',rest)
        detail=self.detail()
        self.assertEqual(base64.b64decode(detail['response']['bodyBase64']),first+rest)
        self.assertEqual(detail['analysis']['responseModel'],'fixture-model')
        self.assertEqual(detail['analysis']['signatureCount'],1)
        self.assertEqual(detail['analysis']['signatureLengths'],[16])
        self.assertEqual(detail['events'][0]['type'],'message_start')
    def test_error_and_redirect_are_preserved(self):
        status,body,h=self.request('POST','/v1/error',b'{}')
        self.assertEqual(status,429)
        self.assertEqual(body,b'{"error":{"type":"fixture","unknown":[1,2]}}')
        self.assertEqual(h['Retry-After'],'7')
        status,body,h=self.request('GET','/v1/redirect')
        self.assertEqual((status,body),(307,b'unchanged'))
        self.assertEqual(h['Location'],'http://127.0.0.1:1/private')
    def test_authentication_isolation_and_export(self):
        self.assertEqual(self.request('GET','/api/config')[0],401)
        self.assertEqual(self.request('GET','/v1/models',headers={'x-api-key':self.state['admin_key']})[0],401)
        status,body,_=self.request('GET','/api/config',admin=True)
        self.assertEqual(json.loads(body)['relayKey'],self.state['relay_key'])
        self.assertNotIn('upstream-secret',body.decode())
        self.assertNotIn(self.state['admin_key'],body.decode())
        self.request('POST','/v1/messages',b'{}')
        self.detail()
        status,body,_=self.request('GET','/api/export',admin=True)
        self.assertEqual(status,200)
        self.assertEqual(len(json.loads(body)['records']),1)
        for secret in ['upstream-secret',self.state['admin_key'],self.state['relay_key']]:
            self.assertNotIn(secret,body.decode())
    def test_truncation_never_truncates_forwarding(self):
        with mock.patch.object(audit,'CAPTURE_LIMIT',512):
            payload=b'q'*14000
            self.assertEqual(self.request('POST','/v1/messages',payload)[:2],(200,payload))
            detail=self.detail()
            self.assertEqual(detail['requestBytes'],14000)
            self.assertEqual(detail['responseBytes'],14000)
            self.assertTrue(detail['requestTruncated'])
            self.assertTrue(detail['responseTruncated'])
            self.assertEqual(len(base64.b64decode(detail['response']['bodyBase64'])),512)
            self.assertEqual(detail['response']['sha256'],hashlib.sha256(payload).hexdigest())
    def test_gzip_payload_kept_and_observed(self):
        status,body,h=self.request('GET','/v1/gzip')
        self.assertEqual(status,200)
        self.assertEqual(h['Content-Encoding'],'gzip')
        self.assertIn(b'gzip-fixture',gzip.decompress(body))
        detail=self.detail()
        self.assertEqual(base64.b64decode(detail['response']['bodyBase64']),body)
        self.assertEqual(detail['analysis']['responseModel'],'gzip-fixture')
    def test_url_join_and_private_address_policy(self):
        for base,expected in [('https://example.com','/v1/messages'),('https://example.com/v1','/v1/messages'),('https://example.com/prefix','/prefix/v1/messages'),('https://example.com/prefix/v1','/prefix/v1/messages')]:
            self.assertEqual(audit.upstream_target(base,'/v1/messages')[3],expected)
        for bad in ['file:///etc/passwd','https://user:secret@example.com','https://example.com?key=secret','https://example.com#fragment']:
            with self.assertRaises(ValueError):audit.parse_base_url(bad)
        with self.assertRaises(ValueError):audit.resolve_addresses('127.0.0.1',80)
        with self.assertRaises(ValueError):audit.upstream_target('https://example.com','/v1/../admin')
    def test_check_models_only_and_concurrent_reads(self):
        status,body,_=self.request('POST','/api/check',{},admin=True)
        self.assertEqual(json.loads(body),{'ok':True,'status':200,'modelCount':1,'error':''})
        self.assertEqual(Fixture.received[-1][:2],('GET','/prefix/v1/models'))
        check_headers={key.lower():value for key,value in Fixture.received[-1][2].items()}
        self.assertEqual(check_headers['anthropic-version'],'2023-06-01')
        self.assertEqual(check_headers['user-agent'],'upstream-audit/1.0')
        self.assertEqual(self.app.records.list()['total'],0)
        def worker(i):
            payload=json.dumps({'model':'fixture','sequence':i}).encode()
            self.assertEqual(self.request('POST','/v1/messages',payload)[:2],(200,payload))
            self.app.records.list()
        with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:list(pool.map(worker,range(12)))
        for _ in range(100):
            records=self.app.records.list()['records']
            if len(records)==12 and all(r['state']!='running' for r in records):break
            time.sleep(.01)
        self.assertEqual(len(records),12)
        self.assertTrue(all(r['state']=='complete' for r in records))
        self.assertEqual(self.request('POST','/api/clear',{'confirm':True},admin=True)[0],200)
        self.assertEqual(self.app.records.list()['total'],0)

    def test_explicit_key_clear_and_short_key_masking(self):
        status,body,_=self.request('PUT','/api/config',{'upstreamApiKey':'xy'},admin=True)
        self.assertEqual(status,200)
        self.assertEqual(json.loads(body)['upstreamKeyMasked'],'••••')
        status,body,_=self.request('PUT','/api/config',{'upstreamApiKey':''},admin=True)
        self.assertEqual(status,200)
        self.assertEqual(self.app.config.snapshot()['upstreamApiKey'],'xy')
        self.assertEqual(self.request('PUT','/api/config',{'clearUpstreamKey':True},admin=True)[0],400)
        status,body,_=self.request('PUT','/api/config',{'enabled':False,'upstreamBaseUrl':'','clearUpstreamKey':True},admin=True)
        self.assertEqual(status,200)
        self.assertFalse(json.loads(body)['hasUpstreamKey'])
        self.assertEqual(self.app.config.snapshot()['upstreamApiKey'],'')

    def test_conflicting_response_framing_does_not_change_payload(self):
        status,body,headers=self.request('GET','/v1/conflicting-framing')
        self.assertEqual((status,body),(200,b'{"unknown":42}'))
        self.assertNotIn('Content-Length',headers)
        self.assertEqual(headers['Transfer-Encoding'],'chunked')

    def test_observer_limits_and_invalid_optional_types(self):
        with mock.patch.object(audit,'MAX_EVENTS',3),mock.patch.object(audit,'CAPTURE_LIMIT',8):
            observer=audit.ResponseObserver([('Content-Type','text/event-stream')])
            observer.feed(b'data: invalid\n\n'*20,10)
            self.assertEqual(len(observer.events),3)
            observer.block({'type':'text','text':'a'*1000})
            self.assertLessEqual(len(''.join(observer.text)),32)
            observer.block({'type':'tool_use','id':None})
            nonstream=audit.ResponseObserver([])
            result=nonstream.finish(b'{"content":null}',{},False)
            self.assertEqual(result['signatureCount'],0)

if __name__=='__main__':
    unittest.main()
