#!/usr/bin/env bash
# Keep the historical entry point; both components share one validated receipt.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "$root/scripts/crawler_business_artifacts.py" build
