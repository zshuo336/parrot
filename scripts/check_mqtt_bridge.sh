#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
case "${1:-}" in
  "") cargo test --locked -p parrot --test test_mqtt_bridge ;;
  --broker)
    command -v "${PARROT_MOSQUITTO_BIN:-mosquitto}" >/dev/null || { echo "Mosquitto executable is required for the broker gate" >&2; exit 1; }
    cargo test --locked -p parrot-remote --test mqtt_mosquitto -- --ignored --nocapture
    ;;
  *) echo "usage: $0 [--broker]" >&2; exit 2 ;;
esac
