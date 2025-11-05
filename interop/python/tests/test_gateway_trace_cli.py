"""The actual CLI must explicitly configure export before starting Ray."""
import contextlib
import io
import subprocess
import sys
import types
import unittest
from unittest.mock import patch

from parrot_protocol import ray_gw


class GatewayTraceCliTests(unittest.TestCase):
    def invoke(self,arguments,initialized=False):
        calls=[];shutdowns=[]
        fake=types.SimpleNamespace(is_initialized=lambda:initialized,
                                   shutdown=lambda:shutdowns.append(True))
        def serve(*args,**kwargs):calls.append((args,kwargs))
        with patch.dict(sys.modules,{'ray':fake}),patch.object(ray_gw,'serve',serve):
            ray_gw.main(['gateway']+arguments,ray_kwargs={'num_cpus':1})
        return calls,shutdowns

    def test_actual_cli_forwards_explicit_export_configuration(self):
        calls,shutdowns=self.invoke(['0','--trace-export-endpoint','https://collector.example/v1/traces',
            '--trace-service-name','orders','--trace-capacity','32','--trace-batch-size','8',
            '--trace-interval','0.03','--trace-request-timeout','1.25','--trace-max-attempts','4',
            '--trace-ca-file','/etc/parrot/collector.pem'])
        self.assertEqual(len(calls),1)
        args,kwargs=calls[0]
        self.assertEqual(args[0],0)
        config=kwargs.get('trace_config')
        self.assertIsNotNone(config,'CLI ignored the explicitly requested export configuration')
        self.assertEqual((config.service_name,config.capacity),('orders',32))
        self.assertEqual((config.export.endpoint,config.export.batch_size,config.export.interval,
            config.export.request_timeout,config.export.max_attempts,config.export.ca_file),
            ('https://collector.example/v1/traces',8,0.03,1.25,4,'/etc/parrot/collector.pem'))
        self.assertEqual(kwargs['ray_kwargs'],{'num_cpus':1})
        self.assertEqual(shutdowns,[True])

    def test_defaults_leave_export_disabled(self):
        calls,_=self.invoke([])
        self.assertEqual(calls[0][0][0],9851)
        self.assertIsNone(calls[0][1].get('trace_config'))

    def test_legacy_registration_and_embedding_ownership_remain_explicit(self):
        calls,shutdowns=self.invoke(['9852','parrot=[::1]:9840'],initialized=True)
        self.assertEqual(calls[0][1]['parrot_addr'],'[::1]:9840')
        self.assertEqual(shutdowns,[])

    def test_export_defaults_are_constructed_only_when_requested(self):
        calls,_=self.invoke(['9851','parrot=localhost:9840',
                             '--trace-export-endpoint','http://localhost:4318/v1/traces'])
        config=calls[0][1].get('trace_config')
        self.assertIsNotNone(config)
        self.assertEqual(config.capacity,256)
        self.assertEqual(config.export.max_attempts,3)
        self.assertEqual(config.export.batch_size,64)
        self.assertEqual(calls[0][1]['parrot_addr'],'localhost:9840')

    def test_invalid_options_are_rejected_before_any_runtime_entry(self):
        cases=[['9851','--trace-export-endpont','http://collector'],
               ['9851','--trace-export-end','http://collector'],
               ['9851','--trace-capacity','32'],['9851','unexpected'],
               ['-1'],['65536'],['9851','parrot=localhost'],
               ['9851','parrot=localhost:0'],['9851','parrot=user:secret@host:90'],
               ['9851','parrot=host:90/path'],
               ['9851','parrot=host:90?'],['9851','parrot=host:90#'],
               ['9851','parrot=host:65536'],['9851','parrot=::1:90'],
               ['9851','parrot=host:90\n'],
               ['9851','--trace-export-endpoint','file:///tmp/spans'],
               ['9851','--trace-export-endpoint','http://collector','--trace-capacity','0'],
               ['9851','--trace-export-endpoint','http://collector','--trace-capacity','4097'],
               ['9851','--trace-export-endpoint','http://collector','--trace-service-name',''],
               ['9851','--trace-export-endpoint','http://collector','--trace-interval','nan'],
               ['9851','--trace-export-endpoint','http://collector','--trace-interval','inf'],
               ['9851','--trace-export-endpoint','http://collector','--trace-batch-size','129'],
               ['9851','--trace-export-endpoint','http://collector','--trace-request-timeout','0'],
               ['9851','--trace-export-endpoint','http://collector','--trace-max-attempts','11']]
        for args in cases:
            with self.subTest(args=args),contextlib.redirect_stderr(io.StringIO()):
                with patch.object(ray_gw,'serve') as serve,patch.dict(sys.modules,{'ray':None}):
                    with self.assertRaises(SystemExit) as raised:
                        ray_gw.main(['gateway']+args)
                    self.assertEqual(raised.exception.code,2)
                    serve.assert_not_called()

    def test_help_does_not_import_or_start_ray(self):
        output=io.StringIO()
        with contextlib.redirect_stdout(output),patch.dict(sys.modules,{'ray':None}):
            with self.assertRaises(SystemExit) as raised:ray_gw.main(['gateway','--help'])
        self.assertEqual(raised.exception.code,0)
        self.assertIn('--trace-export-endpoint',output.getvalue())
        self.assertIn('parrot=',output.getvalue())

    def test_every_optional_trace_setting_requires_an_endpoint(self):
        for name,value in [('service-name','orders'),('capacity','32'),('batch-size','8'),
                           ('interval','0.03'),('request-timeout','1.25'),
                           ('max-attempts','4'),('ca-file','/etc/parrot/collector.pem')]:
            with self.subTest(name=name),contextlib.redirect_stderr(io.StringIO()):
                with patch.object(ray_gw,'serve') as serve,patch.dict(sys.modules,{'ray':None}):
                    with self.assertRaises(SystemExit) as raised:
                        ray_gw.main(['gateway','--trace-'+name,value])
                    self.assertEqual(raised.exception.code,2)
                    serve.assert_not_called()

    def test_explicit_bounds_and_zero_listen_port_reach_serve(self):
        for capacity,batch,interval,timeout,attempts in [(1,1,0.01,0.01,1),(4096,128,60,30,10)]:
            with self.subTest(capacity=capacity):
                calls,_=self.invoke(['0','--trace-export-endpoint','http://localhost:4318/v1/traces',
                    '--trace-capacity',str(capacity),'--trace-batch-size',str(batch),
                    '--trace-interval',str(interval),'--trace-request-timeout',str(timeout),
                    '--trace-max-attempts',str(attempts)])
                self.assertEqual(calls[0][0][0],0)
                config=calls[0][1]['trace_config']
                self.assertEqual((config.capacity,config.export.batch_size,config.export.interval,
                    config.export.request_timeout,config.export.max_attempts),
                    (capacity,batch,interval,timeout,attempts))

    def test_module_help_is_available_in_a_real_process(self):
        python=[sys.executable]+(['-O'] if sys.flags.optimize else [])
        result=subprocess.run(python+['-m','parrot_protocol.ray_gw','--help'],
                              capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertIn('--trace-export-endpoint',result.stdout)
        self.assertNotIn('RAY_GW_PORT=',result.stdout)

    def test_module_invalid_configuration_returns_usage_exit(self):
        for tail in [['--trace-capacity','16'],['--trace-export-endpont','http://collector'],
                     ['--trace-export-endpoint','http://collector','--trace-request-timeout','nan']]:
            with self.subTest(tail=tail):
                python=[sys.executable]+(['-O'] if sys.flags.optimize else [])
                result=subprocess.run(python+['-m','parrot_protocol.ray_gw','9851']+tail,
                                      capture_output=True,text=True,timeout=10)
                self.assertEqual(result.returncode,2,result.stderr)
                self.assertIn('error:',result.stderr)
                self.assertNotIn('Traceback',result.stderr)
                self.assertNotIn('RAY_GW_PORT=',result.stdout)


if __name__=='__main__':unittest.main(verbosity=2)
