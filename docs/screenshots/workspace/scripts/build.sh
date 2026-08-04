#!/bin/sh
set -eu

project="$1"
color="$2"

printf '\033[%sm%s\033[0m  checking sources\n' "$color" "$project"
sleep 0.15
printf '\033[%sm%s\033[0m  compiling\n' "$color" "$project"
sleep 0.2
printf '\033[32m%s  ready\033[0m\n' "$project"
