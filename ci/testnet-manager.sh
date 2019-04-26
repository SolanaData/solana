#!/usr/bin/env bash
set -e

cd "$(dirname "$0")"/..

if [[ -z $BUILDKITE ]]; then
  echo BUILDKITE not defined
  exit 1
fi

if [[ -z $SOLANA_METRICS_PARTIAL_CONFIG ]]; then
  echo SOLANA_METRICS_PARTIAL_CONFIG not defined
  exit 1
fi

if [[ -z $TESTNET ]]; then
  TESTNET=$(buildkite-agent meta-data get "testnet" --default "")
fi

if [[ -z $TESTNET_OP ]]; then
  TESTNET_OP=$(buildkite-agent meta-data get "testnet-operation" --default "")
fi

if [[ -z $TESTNET || -z $TESTNET_OP ]]; then
  (
    cat <<EOF
steps:
  - block: "Manage Testnet"
    fields:
      - select: "Network"
        key: "testnet"
        options:
          - label: "testnet"
            value: "testnet"
          - label: "testnet-perf"
            value: "testnet-perf"
          - label: "testnet-edge"
            value: "testnet-edge"
          - label: "testnet-edge-perf"
            value: "testnet-edge-perf"
          - label: "testnet-beta"
            value: "testnet-beta"
          - label: "testnet-beta-perf"
            value: "testnet-beta-perf"
          - label: "testnet-demo"
            value: "testnet-demo"
      - select: "Operation"
        key: "testnet-operation"
        default: "sanity-or-restart"
        options:
          - label: "Create new testnet nodes and then start network software.  If nodes are already created, they will be deleted and then re-created."
            value: "create-and-start"
          - label: "Create new testnet nodes, but do not start network software.  If nodes are already created, they will be deleted and then re-created."
            value: "create"
          - label: "Start network software on already-created testnet nodes.  If software is already running, it will be restarted."
            value: "start"
          - label: "Stop network software without deleting testnet nodes"
            value: "stop"
          - label: "Update the network software.  Restart network software on failure"
            value: "update-or-restart"
          - label: "Sanity check.  Restart network software on failure"
            value: "sanity-or-restart"
          - label: "Sanity check only"
            value: "sanity"
          - label: "Delete all nodes on a testnet.  Network software will be stopped first if it is running"
            value: "delete"
  - command: "ci/$(basename "$0")"
    agents:
      - "queue=$BUILDKITE_AGENT_META_DATA_QUEUE"
EOF
  ) | buildkite-agent pipeline upload
  exit 0
fi

export SOLANA_METRICS_CONFIG="db=$TESTNET,$SOLANA_METRICS_PARTIAL_CONFIG"
echo "SOLANA_METRICS_CONFIG: $SOLANA_METRICS_CONFIG"

ci/channel-info.sh
eval "$(ci/channel-info.sh)"

case $TESTNET in
testnet-edge|testnet-edge-perf)
  CHANNEL_OR_TAG=edge
  CHANNEL_BRANCH=$EDGE_CHANNEL
  ;;
testnet-beta|testnet-beta-perf)
  CHANNEL_OR_TAG=beta
  CHANNEL_BRANCH=$BETA_CHANNEL
  ;;
testnet|testnet-perf)
  CHANNEL_OR_TAG=$STABLE_CHANNEL_LATEST_TAG
  CHANNEL_BRANCH=$STABLE_CHANNEL
  ;;
testnet-demo)
  CHANNEL_OR_TAG=beta
  CHANNEL_BRANCH=$BETA_CHANNEL
  ;;
*)
  echo "Error: Invalid TESTNET=$TESTNET"
  exit 1
  ;;
esac

if [[ -n $TESTNET_DB_HOST ]]; then
  SOLANA_METRICS_PARTIAL_CONFIG="host=$TESTNET_DB_HOST,$SOLANA_METRICS_PARTIAL_CONFIG"
fi

export SOLANA_METRICS_CONFIG="db=$TESTNET,$SOLANA_METRICS_PARTIAL_CONFIG"
echo "SOLANA_METRICS_CONFIG: $SOLANA_METRICS_CONFIG"
source scripts/configure-metrics.sh

if [[ -n $TESTNET_TAG ]]; then
  CHANNEL_OR_TAG=$TESTNET_TAG
else

  if [[ $BUILDKITE_BRANCH != "$CHANNEL_BRANCH" ]]; then
    (
      cat <<EOF
steps:
  - trigger: "$BUILDKITE_PIPELINE_SLUG"
    async: true
    build:
      message: "$BUILDKITE_MESSAGE"
      branch: "$CHANNEL_BRANCH"
      env:
        TESTNET: "$TESTNET"
        TESTNET_OP: "$TESTNET_OP"
EOF
  ) | buildkite-agent pipeline upload
  exit 0
fi


sanity() {
  echo "--- sanity $TESTNET"
  case $TESTNET in
  testnet-edge)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -ex
      export NO_LEDGER_VERIFY=1
      export NO_VALIDATOR_SANITY=1
      ci/testnet-sanity.sh edge-testnet-solana-com ec2 us-west-1a
    )
    ;;
  testnet-edge-perf)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -ex
      export REJECT_EXTRA_NODES=1
      export NO_LEDGER_VERIFY=1
      export NO_VALIDATOR_SANITY=1
      ci/testnet-sanity.sh edge-perf-testnet-solana-com ec2 us-west-2b
    )
    ;;
  testnet-beta)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -ex
      export NO_LEDGER_VERIFY=1
      export NO_VALIDATOR_SANITY=1
      ci/testnet-sanity.sh beta-testnet-solana-com ec2 us-west-1a
    )
    ;;
  testnet-beta-perf)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -ex
      export REJECT_EXTRA_NODES=1
      export NO_LEDGER_VERIFY=1
      export NO_VALIDATOR_SANITY=1
      ci/testnet-sanity.sh beta-perf-testnet-solana-com ec2 us-west-2b
    )
    ;;
  testnet)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -ex
      export NO_LEDGER_VERIFY=1
      export NO_VALIDATOR_SANITY=1
      #ci/testnet-sanity.sh testnet-solana-com gce us-east1-c
      ci/testnet-sanity.sh testnet-solana-com ec2 us-west-1a
    )
    ;;
  testnet-perf)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -ex
      export REJECT_EXTRA_NODES=1
      export NO_LEDGER_VERIFY=1
      export NO_VALIDATOR_SANITY=1
      #ci/testnet-sanity.sh perf-testnet-solana-com ec2 us-east-1a
      ci/testnet-sanity.sh perf-testnet-solana-com gce us-west1-b
    )
    ;;
  *)
    echo "Error: Invalid TESTNET=$TESTNET"
    exit 1
    ;;
  esac
}

deploy() {
  declare maybeCreate=$1
  declare maybeStart=$2
  declare maybeStop=$3
  declare maybeDelete=$4

  # Create or recreate the nodes
  if [[ -z $maybeCreate ]]; then
    skipCreate=skip
  else
    skipCreate=""
    echo "--- create $TESTNET"
  fi

  # Start or restart the network software on the nodes
  if [[ -z $maybeStart ]]; then
    skipStart=skip
  else
    skipStart=""
    echo "--- start $TESTNET"
  fi

  # Stop the nodes
  if [[ -n $maybeStop ]]; then
    echo "--- stop $TESTNET"
  fi

  # Delete the nodes
  if [[ -n $maybeDelete ]]; then
    echo "--- delete $TESTNET"
  fi

  case $TESTNET in
  testnet-edge)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -x
      ci/testnet-deploy.sh -p edge-testnet-solana-com -C ec2 -z us-west-1a \
        -t "$CHANNEL_OR_TAG" -n 3 -c 0 -u -P -a eipalloc-0ccd4f2239886fa94 \
        ${skipCreate:+-r} \
        ${skipStart:+-s} \
        ${maybeStop:+-S} \
        ${maybeDelete:+-D}
    )
    ;;
  testnet-edge-perf)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -x
      NO_LEDGER_VERIFY=1 \
      NO_VALIDATOR_SANITY=1 \
      RUST_LOG=solana=warn \
        ci/testnet-deploy.sh -p edge-perf-testnet-solana-com -C ec2 -z us-west-2b \
          -g -t "$CHANNEL_OR_TAG" -c 2 \
          -b \
          ${skipCreate:+-r} \
          ${skipStart:+-s} \
          ${maybeStop:+-S} \
          ${maybeDelete:+-D}
    )
    ;;
  testnet-beta)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -x

      # Build an array to pass as opts to testnet-deploy.sh: "-z zone1 -z zone2 ..."
      GCE_ZONE_ARGS=()
      for val in "${GCE_ZONES[@]}"; do
        GCE_ZONE_ARGS+=("-z $val")
      done

      EC2_ZONE_ARGS=()
      for val in "${EC2_ZONES[@]}"; do
        EC2_ZONE_ARGS+=("-z $val")
      done

      if [[ -n $EC2_NODE_COUNT ]]; then
        if [[ -n $GCE_NODE_COUNT ]] || [[ -n $skipStart ]]; then
          maybeSkipStart="skip"
        fi

        # shellcheck disable=SC2068
        ci/testnet-deploy.sh -p beta-testnet-solana-com -C ec2 ${EC2_ZONE_ARGS[@]} \
          -t "$CHANNEL_OR_TAG" -n "$EC2_NODE_COUNT" -c 0 -u -P -a eipalloc-0f286cf8a0771ce35 \
          ${skipCreate:+-r} \
          ${maybeSkipStart:+-s} \
          ${maybeStop:+-S} \
          ${maybeDelete:+-D}
      fi

      if [[ -n $GCE_NODE_COUNT ]]; then
        # shellcheck disable=SC2068
        ci/testnet-deploy.sh -p beta-testnet-solana-com -C gce ${GCE_ZONE_ARGS[@]} \
          -t "$CHANNEL_OR_TAG" -n "$GCE_NODE_COUNT" -c 0 -P \
          ${skipCreate:+-r} \
          ${skipStart:+-s} \
          ${maybeStop:+-S} \
          ${maybeDelete:+-D} \
          ${EC2_NODE_COUNT:+-x}
      fi
    )
    ;;
  testnet-beta-perf)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -x
      NO_LEDGER_VERIFY=1 \
      NO_VALIDATOR_SANITY=1 \
      RUST_LOG=solana=warn \
        ci/testnet-deploy.sh -p beta-perf-testnet-solana-com -C ec2 -z us-west-2b \
          -g -t "$CHANNEL_OR_TAG" -c 2 \
          -b \
          ${skipCreate:+-r} \
          ${skipStart:+-s} \
          ${maybeStop:+-S} \
          ${maybeDelete:+-D}
    )
    ;;
  testnet)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -x
      NO_VALIDATOR_SANITY=1 \
        ci/testnet-deploy.sh -p testnet-solana-com -C ec2 -z us-west-1a \
          -t "$CHANNEL_OR_TAG" -n 3 -c 0 -u -P -a eipalloc-0fa502bf95f6f18b2 \
          -b \
          ${skipCreate:+-r} \
          ${skipStart:+-s} \
          ${maybeStop:+-S} \
          ${maybeDelete:+-D}
        #ci/testnet-deploy.sh -p testnet-solana-com -C gce -z us-east1-c \
        #  -t "$CHANNEL_OR_TAG" -n 3 -c 0 -P -a testnet-solana-com  \
        #  ${maybeReuseLedger:+-r} \
        #  ${maybeDelete:+-D}
    )
    ;;
  testnet-perf)
    # shellcheck disable=2030
    # shellcheck disable=2031
    (
      set -x
      NO_LEDGER_VERIFY=1 \
      NO_VALIDATOR_SANITY=1 \
      RUST_LOG=solana=warn \
        ci/testnet-deploy.sh -p perf-testnet-solana-com -C gce -z us-west1-b \
          -G "--machine-type n1-standard-16 --accelerator count=2,type=nvidia-tesla-v100" \
          -t "$CHANNEL_OR_TAG" -c 2 \
          -b \
          -d pd-ssd \
          ${skipCreate:+-r} \
          ${skipStart:+-s} \
          ${maybeStop:+-S} \
          ${maybeDelete:+-D}
        #ci/testnet-deploy.sh -p perf-testnet-solana-com -C ec2 -z us-east-1a \
        #  -g \
        #  -t "$CHANNEL_OR_TAG" -c 2 \
        #  ${maybeReuseLedger:+-r} \
        #  ${maybeDelete:+-D}
    )
    ;;
  testnet-demo)
    (
      set -x
      echo "Demo net not yet implemented!"
      exit 1
    )
    ;;
  *)
    echo "Error: Invalid TESTNET=$TESTNET"
    exit 1
    ;;
  esac
}

ENABLED_LOCKFILE="${HOME}/${TESTNET}.is_enabled"
CREATED_LOCKFILE="${HOME}/${TESTNET}.is_created"

create-and-start() {
  rm -f "${CREATED_LOCKFILE}"
  deploy create start
  touch "${CREATED_LOCKFILE}"
}
create() {
  rm -f "${CREATED_LOCKFILE}"
  deploy create
  touch "${CREATED_LOCKFILE}"
}
start() {
  if [[ -f ${CREATED_LOCKFILE} ]]; then
    deploy "" start
  else
    echo "Unable to start ${TESTNET}.  Are the nodes created?
    Re-run ci/testnet-manager.sh with \$TESTNET_OP=create or \$TESTNET_OP=create-and-start"
    exit 1
  fi
}
stop() {
  deploy "" ""
}
delete() {
  deploy "" "" "" delete
  rm -f "${CREATED_LOCKFILE}"
}
enable_testnet() {
  touch "${ENABLED_LOCKFILE}"
}
disable_testnet() {
  rm -f "${ENABLED_LOCKFILE}"
}
is_testnet_enabled() {
  if [[ ! -f ${ENABLED_LOCKFILE} ]]; then
    echo "--- ${TESTNET} is currently disabled.  Enable ${TESTNET} by running ci/testnet-manager.sh with \$TESTNET_OP=enable, then re-run with current settings."
    exit 0
  fi
}

case $TESTNET_OP in
enable)
  enable_testnet
  ;;
disable)
  delete
  disable_testnet
  ;;
create-and-start)
  is_testnet_enabled
  create-and-start
  ;;
create)
  is_testnet_enabled
  create
  ;;
start)
  is_testnet_enabled
  start
  ;;
stop)
  is_testnet_enabled
  stop
  ;;
sanity)
  is_testnet_enabled
  sanity
  ;;
delete)
  is_testnet_enabled
  delete
  ;;
update-or-restart)
  is_testnet_enabled
  if start; then
    echo Update successful
  else
    echo "+++ Update failed, restarting the network"
    $metricsWriteDatapoint "testnet-manager update-failure=1"
    create-and-start
  fi
  ;;
sanity-or-restart)
  is_testnet_enabled
  if sanity; then
    echo Pass
  else
    echo "+++ Sanity failed, updating the network"
    $metricsWriteDatapoint "testnet-manager sanity-failure=1"

    # TODO: Restore attempt to restart the cluster before recreating it
    #       See https://github.com/solana-labs/solana/issues/3774
    if false; then
      if start; then
        echo Update successful
      else
        echo "+++ Update failed, restarting the network"
        $metricsWriteDatapoint "testnet-manager update-failure=1"
        create-and-start
      fi
    else
      create-and-start
    fi
  fi
  ;;
*)
  echo "Error: Invalid TESTNET_OP=$TESTNET_OP"
  exit 1
  ;;
esac

echo --- fin
exit 0
