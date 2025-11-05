"""Validate operator CLI settings before importing or starting Ray."""
import argparse
from urllib.parse import urlsplit

from .trace_recording import TraceConfig, TraceExportConfig


def _port(value):
    try:
        port=int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError('port must be an integer in 0..65535') from error
    if not 0 <= port <= 65535:
        raise argparse.ArgumentTypeError('port must be an integer in 0..65535')
    return port


def _registration(value):
    if not value.startswith('parrot='):
        raise argparse.ArgumentTypeError('registration must be parrot=host:port or parrot=[IPv6]:port')
    address=value[len('parrot='):]
    try:
        if not address or len(address)>2048 or any(ord(c)<=32 or ord(c)==127 for c in address):
            raise ValueError('invalid address')
        parsed=urlsplit('//'+address)
        if (not parsed.hostname or parsed.port is None or not 1 <= parsed.port <= 65535
                or parsed.username is not None or parsed.password is not None
                or parsed.path or '?' in address or '#' in address):
            raise ValueError('invalid address')
    except ValueError as error:
        raise argparse.ArgumentTypeError('registration requires a host and port, without credentials or URL path') from error
    return address


def parse_gateway_arguments(argv):
    parser=argparse.ArgumentParser(prog=argv[0] if argv else 'parrot-ray-gateway',
        description='Run a Parrot Python/Ray gateway. Trace export is disabled unless an endpoint is supplied.',
        allow_abbrev=False)
    parser.add_argument('port',nargs='?',type=_port,default=9851,
                        help='listen port (default: 9851; 0 selects an available port)')
    parser.add_argument('registration',nargs='?',type=_registration,metavar='parrot=HOST:PORT',
                        help='connect to a Parrot node instead of listening; IPv6 uses brackets')
    trace=parser.add_argument_group('trace export')
    trace.add_argument('--trace-export-endpoint',help='enable handler recording and export to this OTLP/HTTP JSON URL')
    trace.add_argument('--trace-service-name',help='service.name attached to exported records (default: parrot-python)')
    trace.add_argument('--trace-capacity',type=int,help='record capacity per owning process, 1..4096 (default: 256)')
    trace.add_argument('--trace-batch-size',type=int,help='maximum spans in one request, 1..128 (default: 64)')
    trace.add_argument('--trace-interval',type=float,help='idle worker interval in seconds, 0.01..60 (default: 0.2)')
    trace.add_argument('--trace-request-timeout',type=float,help='socket timeout in seconds, 0.01..30 (default: 2)')
    trace.add_argument('--trace-max-attempts',type=int,help='attempts per batch, 1..10 (default: 3)')
    trace.add_argument('--trace-ca-file',help='optional HTTPS CA file readable in every owning process')
    args=parser.parse_args(argv[1:] if argv else [])
    record={name:getattr(args,'trace_'+name) for name in ['service_name','capacity']
            if getattr(args,'trace_'+name) is not None}
    export={name:getattr(args,'trace_'+name) for name in ['batch_size','interval','request_timeout','max_attempts','ca_file']
            if getattr(args,'trace_'+name) is not None}
    config=None
    if args.trace_export_endpoint is None:
        if record or export:parser.error('trace options require --trace-export-endpoint')
    else:
        try:
            config=TraceConfig(**record,export=TraceExportConfig(endpoint=args.trace_export_endpoint,**export))
        except (TypeError,ValueError) as error:
            parser.error(str(error))
    return args.port,args.registration,config
