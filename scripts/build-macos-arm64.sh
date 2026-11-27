#!/usr/bin/env bash
# Keep the original command working for existing macOS build instructions.
exec bash "$(dirname -- "${BASH_SOURCE[0]}")/build-macos.sh" "$@"
