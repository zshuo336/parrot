"""Verify the actual JVM measured-metrics producer."""
from gateway_metrics_contract import check_gateway_metrics


def check_jvm_metrics(port):
    check_gateway_metrics(port, 'jvm_threads', 'JVM')
