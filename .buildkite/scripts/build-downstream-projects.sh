#!/usr/bin/env bash

set -e
here=$(dirname "$0")

# shellcheck source=.buildkite/scripts/common.sh
source "$here"/common.sh

agent="${1-solana}"

group "downstream projects" \
  '{ "name": "spl", "command": "./ci/downstream-projects/run-spl.sh", "timeout_in_minutes": 30, "agent": "'"$agent"'" }' \
  '{ "name": "example-helloworld", "command": "./ci/downstream-projects/run-example-helloworld.sh", "timeout_in_minutes": 30, "agent": "'"$agent"'" }'
