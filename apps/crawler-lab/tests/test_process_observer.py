import unittest
from unittest.mock import patch
from process_observer import command_line


class Process:
    def __init__(self, value): self.value = value
    def cmdline(self):
        if isinstance(self.value, Exception): raise self.value
        return self.value


class ProcessObserverContracts(unittest.TestCase):
    def test_exiting_macos_process_keeps_an_explicit_metadata_marker(self):
        process = Process(OSError(0, 'Undefined error: 0 (originated from sysctl(KERN_PROCARGS2))'))
        with patch('process_observer.sys.platform', 'darwin'):
            self.assertEqual(command_line(process), ['<command unavailable: macOS process-exit sysctl race>'])

    def test_unrelated_errors_propagate_and_success_preserves_arguments(self):
        self.assertEqual(command_line(Process(['python3', 'gateway.py'])), ['python3', 'gateway.py'])
        for platform, error in [('darwin', OSError(1, 'permission')), ('darwin', OSError(0, 'different syscall')),
                                ('linux', OSError(0, 'sysctl(KERN_PROCARGS2)'))]:
            with self.subTest(platform=platform, error=error), patch('process_observer.sys.platform', platform):
                with self.assertRaises(OSError): command_line(Process(error))
