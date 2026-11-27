#!/bin/sh
# Makes the TLS certificate for `ait serve --lan` (ECDSA P-256, self-signed,
# valid 10 years) and prints its fingerprint in the form ait expects.
#
#   scripts/make-cert.sh [folder] [--force] [--name host-name]
#
# Default folder: <git-common-dir>/ait/tls (next to the default database;
# never committed). Existing files are kept unless --force is given.
# Needs openssl (Git for Windows ships one in usr/bin).
set -eu

dir=""
force=0
name="ait-host"
while [ $# -gt 0 ]; do
  case "$1" in
    --force) force=1 ;;
    --name) shift; name="${1:?--name needs a value}" ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    -*) echo "make-cert: unknown option $1" >&2; exit 2 ;;
    *) dir="$1" ;;
  esac
  shift
done

if ! command -v openssl >/dev/null 2>&1; then
  echo "make-cert: openssl not found (install it, or use Git Bash / scripts/make-cert.ps1)" >&2
  exit 1
fi

if [ -z "$dir" ]; then
  common=$(git rev-parse --git-common-dir 2>/dev/null) || {
    echo "make-cert: not inside a git repository; give the folder: scripts/make-cert.sh <folder>" >&2
    exit 2
  }
  dir="$(cd "$common" && pwd)/ait/tls"
fi

cert="$dir/cert.pem"
key="$dir/key.pem"
if [ "$force" -eq 0 ] && { [ -e "$cert" ] || [ -e "$key" ]; }; then
  echo "make-cert: $cert or $key already exists; pass --force to replace them" >&2
  echo "           (then restart ait serve --lan and update remote_fingerprint on every PC)" >&2
  exit 1
fi

mkdir -p "$dir"
umask 077
# MSYS_NO_PATHCONV keeps Git Bash from turning /CN=... into a Windows path.
MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' openssl req -x509 -newkey ec \
  -pkeyopt ec_paramgen_curve:prime256v1 -nodes -sha256 -days 3650 \
  -subj "/CN=$name" -keyout "$key" -out "$cert" 2>/dev/null || {
  echo "make-cert: openssl failed" >&2
  exit 1
}

fp=$(openssl x509 -in "$cert" -noout -fingerprint -sha256 | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f')
echo "certificate: $cert"
echo "key:         $key  (keep it on this PC only)"
echo "fingerprint: sha256:$fp"
echo
echo "On every agent PC, in default.cfg:  remote_fingerprint = sha256:$fp"
