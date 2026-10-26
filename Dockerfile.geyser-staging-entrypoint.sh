#!/bin/sh
set -eu

chown 10001:10001 /data
cp /seed/config.yml /data/config.yml
chown 10001:10001 /data/config.yml
exec su-exec geyser java -Xms128M -Xmx512M -jar /opt/geyser/Geyser-Standalone.jar
