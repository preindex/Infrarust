#!/bin/bash
set -Eeuo pipefail

geyser_config="${GEYSER_CONFIG_FILE:-/app/config/geyser/config.yml}"
geyser_data="${GEYSER_DATA_DIR:-/app/geyser}"
geyser_enabled="${GEYSER_ENABLED:-auto}"
geyser_java_opts=( -Xms128M -Xmx512M )

case "$geyser_enabled" in
  auto)
    if [[ -e "$geyser_config" && ! -r "$geyser_config" ]]; then
      echo "Geyser config exists but is unreadable: $geyser_config" >&2
      exit 1
    elif [[ -r "$geyser_config" ]]; then
      geyser_enabled=true
    else
      geyser_enabled=false
      echo "Geyser is disabled because no readable config was found at $geyser_config. Set GEYSER_ENABLED=true to require it." >&2
    fi
    ;;
  true|false) ;;
  *)
    echo "GEYSER_ENABLED must be auto, true, or false (got: $geyser_enabled)" >&2
    exit 2
    ;;
esac

if [[ "$geyser_enabled" == true && ! -r "$geyser_config" ]]; then
  echo "Geyser is enabled but its config is missing or unreadable: $geyser_config" >&2
  echo "Mount a Geyser config at /app/config/geyser/config.yml or set GEYSER_CONFIG_FILE." >&2
  exit 1
fi

if [[ "$geyser_enabled" == true ]]; then
  mkdir -p "$geyser_data"
  if [[ "$geyser_config" != "$geyser_data/config.yml" ]]; then
    cp "$geyser_config" "$geyser_data/config.yml"
  fi

  if [[ -n "${GEYSER_JAVA_OPTS:-}" ]]; then
    read -r -a extra_java_opts <<< "$GEYSER_JAVA_OPTS"
    geyser_java_opts+=("${extra_java_opts[@]}")
  fi
  echo "Starting Geyser with config $geyser_config"
else
  echo "Starting Infrarust without Geyser"
fi

infrarust_pid=
geyser_pid=
service_pids=()

force_stop() {
  for pid in "${service_pids[@]}"; do
    kill -KILL "$pid" 2>/dev/null || true
  done
}

stop_children() {
  local pid watchdog_dir
  ((${#service_pids[@]} > 0)) || return 0

  for pid in "${service_pids[@]}"; do
    kill -TERM "$pid" 2>/dev/null || true
  done

  # Keep the supervisor inside Docker's 45 second grace period even if a child
  # fails to stop after SIGTERM. A second SIGTERM force-stops both children.
  watchdog_dir="$(mktemp -d /tmp/infrarust-stop.XXXXXX)"
  (sleep 40; [[ ! -e "$watchdog_dir/done" ]] && force_stop) &
  for pid in "${service_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  touch "$watchdog_dir/done"
}

shutdown() {
  trap force_stop TERM INT
  stop_children
  exit 0
}

trap shutdown TERM INT

/sbin/infrarust "$@" &
infrarust_pid=$!
service_pids+=("$infrarust_pid")
if [[ "$geyser_enabled" == true ]]; then
  (cd "$geyser_data" && exec java "${geyser_java_opts[@]}" -jar /opt/geyser/Geyser-Standalone.jar) &
  geyser_pid=$!
  service_pids+=("$geyser_pid")
fi

set +e
wait -n -p exited_pid "${service_pids[@]}"
status=$?
set -e

# Either enabled service exiting ends the combined container. Stop the other
# service and return the original failure; a clean but unexpected exit is failure.
remaining_pids=()
for pid in "${service_pids[@]}"; do
  [[ "$pid" == "${exited_pid:-}" ]] || remaining_pids+=("$pid")
done
service_pids=("${remaining_pids[@]}")
stop_children

if (( status == 0 )); then
  exit 1
fi
exit "$status"
