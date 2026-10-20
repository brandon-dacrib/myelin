#!/bin/sh
# The entrypoint of Myelin's matrimail image, after mautrix's docker-run.sh: started as root,
# it makes /data the bridge's (UID:GID, 1337 by default) and runs the bridge as that user on
# /data/config.yaml. Any arguments replace the default ones (`--help`, `--version`,
# `--generate-example-config`), so `docker run <image> --help` works.
set -eu
if [ "$#" -eq 0 ]; then
    if [ ! -f /data/config.yaml ]; then
        echo "matrimail: /data/config.yaml is missing. Myelin writes it (and registration.yaml" >&2
        echo "and passphrase) for a bridge it offers; for one run by hand, save the files the" >&2
        echo "bridge's page shows into the directory mounted at /data." >&2
        exit 1
    fi
    set -- --config /data/config.yaml --registration /data/registration.yaml
fi
# matrimail's fallback locations (./data/passphrase, ~/.config/matrimail) resolve under the
# working directory and the home: both on the volume, never under root's home.
cd /
export HOME=/data
if [ "$(id -u)" = 0 ]; then
    chown -R "${UID}:${GID}" /data
    exec setpriv --reuid="${UID}" --regid="${GID}" --clear-groups /usr/bin/matrimail "$@"
fi
exec /usr/bin/matrimail "$@"
