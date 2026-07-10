# Reproduction commands for the Phoenix -> Lexe routing failure.
# See README.md for the full analysis.

eclair_dir := "eclair-checkout"
eclair_commit := "7fb9460183490260537c2e80c0ce4f1af144ea90"

# List available recipes
default:
    @just --list

# Reproduce the eclair routing failure (clones eclair; requires JDK 21+)
repro: _clone-eclair
    #!/usr/bin/env bash
    set -euo pipefail
    java_bin="${JAVA_HOME:+$JAVA_HOME/bin/}java"
    java_major=$("$java_bin" -version 2>&1 | sed -nE 's/.*version "([0-9]+).*/\1/p')
    if [ "${java_major:-0}" -lt 21 ]; then
        echo "error: eclair requires JDK 21+, found ${java_major:-none}." >&2
        echo "Set JAVA_HOME to a JDK 21 installation and re-run." >&2
        exit 1
    fi
    cp eclair/LexeGraphDebugSpec.scala \
        {{eclair_dir}}/eclair-core/src/test/scala/fr/acinq/eclair/router/
    cd {{eclair_dir}}
    LEXE_GRAPH_CSV="$PWD/../data/graph.csv" \
        ./mvnw -pl eclair-core test \
        -Dsuites='fr.acinq.eclair.router.LexeGraphDebugSpec'
    echo
    echo "Key output above:"
    echo " - 'prod-first-attempt ...' cases (eclair's actual first-attempt"
    echo "   config) print NO ROUTE for every amount: the reproduced failure."
    echo " - 'candidate path ...' lines show the k-shortest paths and their"
    echo "   full-amount fees vs the 1,402,832 msat budget."

# Fetch the pinned eclair commit into {{eclair_dir}} (no-op if present)
_clone-eclair:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -d {{eclair_dir}}/.git ]; then exit 0; fi
    git init -q {{eclair_dir}}
    cd {{eclair_dir}}
    git remote add origin https://github.com/ACINQ/eclair
    git fetch --depth 1 origin {{eclair_commit}}
    git checkout -q FETCH_HEAD

# Run the LDK-side graph-debug CLI, e.g. `just ldk stats`
ldk *args:
    cargo run --manifest-path ldk/Cargo.toml -- {{args}}

# LDK baseline: an in-budget route exists for the actual failed invoice
ldk-baseline:
    just ldk route-bolt12 data/bolt12_invoice.txt --max-cltv 576

# The Lexe LSP's channels, policies, and scorer liquidity estimates
ldk-lexe-node:
    just ldk node 0314a77523d1dcbc5db56081edcbc24ab820b35e343a6c6769176de707c178d457

# All ACINQ -> peer -> Lexe 2-hop paths with eclair-style fee accounting
ldk-two-hop:
    just ldk two-hop \
        03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f \
        0314a77523d1dcbc5db56081edcbc24ab820b35e343a6c6769176de707c178d457 \
        --amount-msat 350757124
