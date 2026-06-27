#!/usr/bin/env bash
# One-shot: provision the server, then install + start Hermes on it.
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
"$HERE/provision.sh"
"$HERE/bootstrap.sh"
