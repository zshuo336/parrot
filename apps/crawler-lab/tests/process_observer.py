"""Command-line metadata is optional; process birth identity remains mandatory."""
import sys


def command_line(process):
    try:
        return process.cmdline()
    except OSError as error:
        # macOS sysctl can fail with errno 0 while a process is exiting. Do not
        # discard its already sampled PID/create_time or suppress other errors.
        if sys.platform == 'darwin' and error.errno == 0 and 'sysctl(KERN_PROCARGS2)' in str(error):
            return ['<command unavailable: macOS process-exit sysctl race>']
        raise
