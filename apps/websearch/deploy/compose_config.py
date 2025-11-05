#!/usr/bin/env python3
"""Prepare a complete Compose model without shell parsing application arguments."""
import json
import sys


VALUES = {'--pages', '--sites', '--maxdepth', '--port', '--data', '--bind',
          '--wait', '--node-root', '--crawl-origin'}


def tokens(arguments):
    index = 0
    while index < len(arguments):
        option = arguments[index]
        if option == '--':
            yield None, arguments[index:]
            return
        count = 2 if option in VALUES else 1
        part = arguments[index:index + count]
        if count == 2 and (len(part) != 2 or not part[1] or part[1].startswith('--')):
            raise ValueError(f'{option} requires a value')
        yield option, part
        index += count


def configure(model, extra):
    pieces = list(tokens(extra))
    if any(option in ('--help', '-h', '--validate-only') for option, _ in pieces):
        raise ValueError('up requires running arguments; use the application --help for CLI usage')
    overridden = {option for option, _ in pieces if option is not None and option.startswith('--')}
    app = model['services']['app']
    base = app['command']
    command = [value for option, part in tokens(base) if option not in overridden for value in part]
    # Compose config escapes dollars for round-trip interpolation; only newly
    # introduced literal argv need escaping here. Preserve duplicate user
    # options so the real Rust CLI, not a last-wins merge, rejects them.
    command += [value.replace('$', '$$') for value in extra]
    old_port = base[base.index('--port') + 1]
    port_text = command[command.index('--port') + 1]
    if not port_text.isascii() or not port_text.isdecimal() or not 1 <= int(port_text) <= 65535:
        raise ValueError('--port must be 1..65535 for container port publication; use WS_WEB_PORT=0 for a random host port')
    port = int(port_text)
    for published in app.get('ports', []):
        if int(published['target']) == int(old_port):
            published['target'] = port
    app['command'] = command
    app.setdefault('environment', {})['WS_WEB_LISTEN_PORT'] = str(port)
    return model


def healthy(value):
    return isinstance(value, dict) and all(type(value.get(key)) is int and value[key] >= 0
        for key in ('docs', 'terms', 'postings', 'queries', 'segments'))


def main():
    mode = sys.argv[1]
    if mode == 'render':
        json.dump(configure(json.load(sys.stdin), sys.argv[2:]), sys.stdout)
    elif mode == 'argv':
        with open(sys.argv[2]) as source:
            command = json.load(source)['services']['app']['command']
        for value in command:
            sys.stdout.buffer.write(value.replace('$$', '$').encode() + b'\0')
    elif mode == 'health':
        if not healthy(json.load(sys.stdin)):
            raise ValueError('invalid Websearch statistics')
    else:
        raise ValueError('unknown operation')


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, IndexError, TypeError) as error:
        print(f'compose configuration: {error}', file=sys.stderr)
        sys.exit(2)
