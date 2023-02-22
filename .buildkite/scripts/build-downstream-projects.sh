#!/usr/bin/env bash

here=$(dirname "$0")

# shellcheck source=.buildkite/scripts/common.sh
source "$here"/common.sh

agent="${1-solana}"

group "downstream projects" \
  '{ "name": "spl", "command": "./ci/downstream-project/run-spl.sh", "timeout_in_minutes": 30, "agent": "'"$agent"'" }' \
  '{ "name": "openbook-dex", "command": "./ci/downstream-project/run-openbook-dex.sh", "timeout_in_minutes": 30, "agent": "'"$agent"'" }' \
  '{ "name": "example-helloworld", "command": "./ci/downstream-project/run-example-helloworld.sh", "timeout_in_minutes": 30, "agent": "'"$agent"'" }'
