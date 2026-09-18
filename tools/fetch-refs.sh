#!/usr/bin/env bash
# Shallow-clone the reference codebases every track brief points at, into ./refs/.
# They are references and (where the license allows) quarries, never bases. refs/ is git-ignored.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p refs
clone() { [ -d "refs/$2" ] || git clone --depth 1 -q "$1" "refs/$2"; echo "refs/$2"; }
clone https://github.com/element-hq/synapse.git synapse            # AGPL-3.0: behavioral reference only, no code copying
clone https://github.com/ruma/ruma.git ruma                        # MIT
clone https://github.com/palpo-im/palpo.git palpo                  # Apache-2.0
clone https://gitlab.com/famedly/conduit.git conduit               # Apache-2.0 (upstream Conduit)
clone https://github.com/mautrix/go.git mautrix-go                 # MPL-2.0
clone https://github.com/mautrix/python.git mautrix-python         # MPL-2.0
clone https://github.com/matrix-org/complement.git complement      # Apache-2.0
clone https://github.com/matrix-org/matrix-spec.git matrix-spec    # Apache-2.0 (OpenAPI under data/api)
clone https://github.com/matrix-org/sytest.git sytest              # Apache-2.0
