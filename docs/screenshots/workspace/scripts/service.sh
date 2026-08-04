#!/bin/sh
set -eu

name="$1"
message="$2"

printf '\033[36m%s\033[0m  starting\n' "$name"
sleep 0.15
printf '\033[32m%s\033[0m\n' "$message"

while :; do
  sleep 30
done
