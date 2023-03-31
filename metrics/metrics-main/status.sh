#!/bin/bash

# List of containers
containers=("chronograf_8889" "grafana" "alertmanager" "alertmanager-discord" "prometheus" "chronograf" "kapacitor")

# Discord webhook
discord_webhook="$DISCORD_WEBHOOK"

# Send a message to Discord
send_discord_message() {
  local message="$1"
  curl -sS -H "Content-Type: application/json" -X POST -d "{\"content\": \"$message\"}" "$discord_webhook_url"
}

# Iterate over the containers and check their status
for container in "${containers[@]}"; do
  container_status=$(docker inspect --format '{{.State.Status}}' "$container" 2>/dev/null)

  if [ "$container_status" != "running" ]; then
    send_discord_message "$container is down and it's being redeployed..."

    # Run the container.sh script to redeploy the container
    chmod +x "$container.sh"
    ./"$container.sh"
    sleep 10

    # Check the container status again
    container_status=$(docker inspect --format '{{.State.Status}}' "$container" 2>/dev/null)

    if [ "$container_status" != "running" ]; then
      send_discord_message "$container failed to redeploy and manual intervention is required"
    else
      send_discord_message "$container has been redeployed successfully"
    fi
  fi
done
