# Phoenix → Lexe payment failures: root cause analysis & repro

Investigation of [lexe-app/lexe-public#79](https://github.com/lexe-app/lexe-public/issues/79):
payments from Phoenix to a Lexe wallet repeatedly failed with a "fee too high"
error, even after the payer raised their max-fee allowance, and even for small
amounts. This repo contains the analysis and a runnable reproduction tracing
the failure to a deterministic behavior in eclair's multi-part route
calculation on ACINQ's trampoline node, replayed offline against a snapshot of
the public network graph.

**TL;DR**: ACINQ's trampoline node rejects these payments with
`trampoline_fee_insufficient` without attempting a single HTLC, even though
multiple routes satisfying the fee budget exist. On the first (and, for
route-not-found, only) attempt, eclair's MPP route calculation assigns the full
amount to the minimum-*weight* path — which its success-probability heuristic
ranks first despite having the most expensive fees — then validates the fee
budget only after splitting, and the fallback that would try cheaper paths is
gated on `randomize`, which is disabled on the first attempt. The recipient's
0.3% payment-path fee consumes 75% of Phoenix's fixed 0.4% trampoline fee
budget, which makes the remaining margin tighter than the divergence between
"minimum weight" and "minimum fee" — so the deterministic first attempt always
busts the budget and the payment always fails.

**Update 2026-07-11**: the 0.3% fee turned out to be stale channel state on
Lexe's side and has been fixed — new Lexe invoices advertise a 0-fee payment
path (see [the postscript](#postscript-the-03-fee-and-the-lexe-side-fix-2026-07-11)).
The eclair findings below stand on their own: even at 0.3%, the fee budget
admitted several routes, and the payment should never have failed.

## Repository contents

```
data/2026_07_09-network_graph  serialized LDK NetworkGraph, dumped 2026-07-09
                               from Lexe's LSP (public gossip data)
data/2026_07_09-prob_scorer    serialized LDK ProbabilisticScorer (the LSP's
                               liquidity estimates), same snapshot
data/graph.csv                 the graph exported as CSV, one row per directed
                               edge (produced by `graph-debug export`)
data/bolt12_invoice.txt        one of the actual BOLT12 invoices Phoenix
                               received from the Lexe recipient (349,708 sat;
                               blinded path, so it reveals no recipient info)
ldk/                           Rust CLI: inspect the graph/scorer, parse
                               invoices, run LDK's router with Phoenix's
                               trampoline budgets, export the CSV
eclair/LexeGraphDebugSpec.scala  ScalaTest spec that loads graph.csv into
                               eclair's router and replays its route
                               calculation with the trampoline relay's exact
                               parameters
```

## Symptoms

From the payer's Phoenix logs (Android, mainnet, block height ~957,338,
2026-07-09 14:33–16:13 log time):

- 8 payment attempts to the same Lexe wallet, via BOLT12 offer (7) and BOLT11
  invoice (1), amounts 79,119 / 158,239 / 343,378 / 349,708 / 349,853 sat.
- Every attempt: HTLC sent to ACINQ's trampoline node
  (`03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f`),
  `UpdateFailHtlc` received back **~100–165 ms after the HTLC was irrevocably
  committed** — far too fast for any downstream HTLC round trip.
- Lexe's LSP saw **no incoming HTLCs** for these payment hashes: ACINQ never
  forwarded anything.
- BOLT12 offer resolution worked fine: the `invoice_request` onion message
  round-tripped through ACINQ to the Lexe wallet and a valid invoice came back
  in ~500 ms. Only the HTLC leg failed.
- A control payment the previous day from the same Phoenix wallet to a
  different (non-Lexe) recipient succeeded, paying ACINQ **exactly** its
  minimum trampoline fee (1,295,768 msat = 4 sat + 0.4% of 322,942 sat), so the
  trampoline itself and the fee tier were healthy.

Phoenix's wallet params contain a **single** trampoline fee tier
(`fee_base=4 sat, fee_proportional=4000 ppm, cltv_expiry_delta=576`), so
lightning-kmp cannot retry with a higher fee: on `TrampolineFeeInsufficient`
it immediately fails with `RetryExhausted` and the part failure
`NotEnoughFees` — surfaced to the user as "fee too high". The in-app max-fee
setting never reaches ACINQ; the offered trampoline fee is fixed at 0.4% + 4 sat.

## The fee math

For the 349,708 sat attempt:

| | msat |
|---|---|
| amount to deliver | 349,708,000 |
| Phoenix pays ACINQ (amount + trampoline fee) | 351,110,832 |
| **total budget for everything past ACINQ** | **1,402,832** |
| Lexe's payment-path fee (0 + 3000 ppm ≈ 0.3%) | 1,049,124 |
| **remaining for ACINQ's own route to the Lexe LSP** | **353,708 (~0.101%)** |

The Lexe invoices advertise a blinded path with intro node = Lexe's LSP
(`0314a77523d1dcbc5db56081edcbc24ab820b35e343a6c6769176de707c178d457`,
alias `Lexe.app`), 2 blinded hops, `payinfo = {fee_base: 0, fee_prop: 3000 ppm,
cltv_delta: 114}`. The BOLT11 invoice's route hint has the same 0.3% fee.

## What was ruled out

Using the graph + scorer snapshot in `data/`:

- **Liquidity**: the LSP's scorer estimated ~3.9M sat available on the cheapest
  inbound hop (10× the payment), with several other inbound channels holding
  0.5–19M sat estimated liquidity.
- **Gossip staleness**: all 25 of the Lexe LSP's public channels had
  both-direction `channel_update`s ≤ 6 days old — nothing near the 14-day prune
  horizon.
- **Route existence**: LDK's router (as a neutral reference) finds a route
  ACINQ → 1sats.com (1 msat fee) → Lexe LSP → blinded path at **total fee
  1,049,125 msat ≤ 1,402,832 budget**, for every attempted amount, even capped
  at eclair's CLTV budget of 576.
- **Fee headroom under eclair's accounting**: eclair's trampoline relay counts
  its *own* first-hop channel fee against the budget
  (`includeLocalChannelCost = true`). Even so, enumerating all
  ACINQ → peer → Lexe two-hop paths shows at least five that fit, e.g. via
  HODLmeTight: 176,033 (ACINQ's own fee) + 10,522 (peer) + 1,049,124 (blinded)
  = **1,235,679 ≤ 1,402,832**.

So the public graph supports these payments with margin. The failure had to be
inside eclair's route calculation or ACINQ's node state.

## Reproducing eclair's decision

`eclair/LexeGraphDebugSpec.scala` loads `data/graph.csv` (~62k directed edges
after dropping disabled ones) into eclair's own router types and invokes
`RouteCalculation.findRoute` / `findMultiPartRoute` with **exactly** the
parameters eclair's trampoline relay uses
([`NodeRelay.computeRouteParams`](https://github.com/ACINQ/eclair/blob/master/eclair-core/src/main/scala/fr/acinq/eclair/payment/relay/NodeRelay.scala)):
`maxFeeFlat = amountIn − amountOut = 1,402,832 msat`, `maxFeeProportional = 0`,
`maxCltv = 576`, `includeLocalChannelCost = true`, reference.conf default
heuristics, block height 957,338. The blinded path is modeled the same way
`computeTarget` does (virtual edge from the intro node, fee 0+3000 ppm,
`balance_opt = htlc_max`). Tested at eclair master `7fb9460` (2026-07-08).

Results:

| configuration | result |
|---|---|
| single-part, default heuristics | route found, fee 1,235,679 ✓ |
| single-part, fee-only weights | route found, fee 1,225,151 ✓ |
| multi-part, `randomize=true` (retry config), 100 runs | **~50% RouteNotFound** |
| multi-part, `randomize=false` + `FullCapacity` (**first-attempt config**) | **RouteNotFound, 100% deterministic** — every amount, every balance assumption |

The first-attempt configuration matters because
[`MultiPartPaymentLifecycle`](https://github.com/ACINQ/eclair/blob/master/eclair-core/src/main/scala/fr/acinq/eclair/payment/send/MultiPartPaymentLifecycle.scala)
forces it:

```scala
case Event(r: SendMultiPartPayment, _) =>
  // we don't randomize the first attempt, regardless of configuration choices
  val routeParams = r.routeParams.copy(randomize = false,
    mpp = r.routeParams.mpp.copy(splittingStrategy = FullCapacity))
```

and on `PaymentRouteNotFound` with nothing in the ignore list (always true on a
first attempt), it **fails the payment immediately** — the retry branch only
runs when previously-ignored channels exist. `NodeRelay.translateError` then
maps `RouteNotFound → TrampolineFeeInsufficient`. This exactly matches the
observed behavior: deterministic, instant (~100 ms), identical failures with no
HTLC ever sent downstream.

## The mechanism

`findMultiPartRouteInternal` searches for k-shortest paths sized for the MPP
*part* amount (`total / max-parts` = 69,941,600 msat), ranked by heuristic
**weight** — which includes amount-scaled virtual hop costs and a
failure-probability penalty that strongly favors high-capacity channels — then
splits the total across those paths and only *afterwards* validates the total
**fee** against the budget.

The five candidate paths it finds for the part amount, with their fee at the
full 350.7M msat (budget 1,402,832):

| weight rank | first hop (ACINQ's own fee) | fee @ full amount | fits? |
|---|---|---|---|
| 1 | → 1sats.com (1000 + 1499 ppm) | 1,575,909 | ✗ |
| 2 | → 1sats.com, 2nd channel | 1,575,908 | ✗ |
| 3 | → SatLink (1000 + 999 ppm) | 1,400,880 | ✓ |
| 4 | → HODLmeTight (1000 + 499 ppm) | 1,235,679 | ✓ |
| 5 | → CoinGate | 1,401,881 | ✓ |

ACINQ's channel to 1sats.com is enormous (its own balance estimate spans tens
of billions of msat), so the failure-cost heuristic ranks it first even though
its 0.15% fee is triple that of path 4. The `FullCapacity` splitting strategy
then assigns the **entire** amount to path 1 (its capacity easily allows it),
`validateMultiPartRoute` rejects the total fee (1,575,909 > 1,402,832), and the
"retry with weight-sorted paths" fallback inside `findMultiPartRouteInternal`
is skipped because it's gated on `routeParams.randomize` — false on the first
attempt. `RouteNotFound`, payment dead, even though paths 3–5 fit the budget.

Why only Lexe-bound payments trip this: with a typical ~0-fee recipient
payment path, the sender-side margin is the full 0.4% and even the
weight-preferred path validates. Lexe's 0.3% path fee shrinks the margin to
~0.1%, which is smaller than the fee spread among eclair's top-weight
candidates.

## Suggested fixes

**eclair** (any one of these would resolve it):
- Run the cheaper-path fallback in `findMultiPartRouteInternal` regardless of
  `randomize` (sorting by fee rather than weight would help further).
- Make the split fee-aware: skip/deprioritize candidate paths whose
  full-amount fee busts the remaining budget when cheaper candidates exist.
- Retry the route request at least once after a first-attempt
  `PaymentRouteNotFound` in `MultiPartPaymentLifecycle` (later attempts use
  `randomize = true`, which succeeds ~50% of the time per draw here).

**Phoenix / lightning-kmp**: a single trampoline fee tier turns any marginal
routing failure into a hard, non-retriable "fee too high". Additional tier(s),
or honoring the user's max-fee setting when offering the trampoline fee, would
make this class of failure recoverable.

**Lexe** (mitigation, no upstream dependency): lower the fee advertised in
blinded paths / route hints. At 0.1%, even the worst-ranked candidate path
above totals well under the budget, so eclair's deterministic first attempt
succeeds. *Done 2026-07-11 — the advertised fee is now 0; see the postscript
below.*

## Postscript: the 0.3% fee, and the Lexe-side fix (2026-07-11)

The advertised 0.3% turned out to be stale state, not intent. Lexe had lowered
its configured LSP → user forwarding fee from 3000 ppm to 0 back in April
2026 — but in LDK, a channel's config is fixed at open, so changing the default
only affects *new* channels, and Lexe's migration path for existing channels
was disabled in production. The recipient's oldest channel (opened September
2025) still carried 3000 ppm. A Lexe node advertises the **max** fee across the
LSP's currently-configured fee and each existing channel's last
`channel_update`, so that single stale channel poisoned every invoice this
recipient generated — while users with only post-April channels advertised 0
and were unaffected. (The max is there because a channel's stored config is
what its owner nominally enforces; the fix had to be applied on the LSP side
rather than in the advertisement.)

On 2026-07-11, Lexe migrated all remaining stale channels to 0 ppm. Newly
generated BOLT11 invoices and BOLT12 invoices now
advertise a 0-fee payment path, handing eclair the full 0.4% trampoline budget:
every candidate path fits with wide margin, so this failure mode is gone for
payments to Lexe wallets regardless of any eclair-side change.

One more data point for the eclair analysis: on 2026-07-11, *before* the fix,
an identical 350k sat payment against a 0.3% invoice **succeeded**. The first
attempt is deterministic *given* ACINQ's graph and balance-estimate state, but
that state evolves continuously — so with a squeezed budget, the failure
presents in the wild as recipient-specific flakiness rather than a hard outage.
The snapshot in `data/` reproduces the failing state. Any recipient whose
final-hop fee leaves the sender less margin than the fee spread among eclair's
top-weight candidates can still trip this.

## Running the repro

With [`just`](https://github.com/casey/just) installed:

```bash
# The actual reproduction: shallow-clones eclair at the pinned commit,
# drops in the spec, and runs it. Requires JDK 21+ (set JAVA_HOME).
just repro

# LDK-side commands (require Rust):
just ldk-baseline   # LDK finds an in-budget route for the failed invoice
just ldk-lexe-node  # Lexe LSP channels, policies, scorer liquidity estimates
just ldk-two-hop    # ACINQ -> peer -> Lexe paths, eclair-style fee accounting
just ldk <args>     # any graph-debug subcommand, e.g. `just ldk stats`
```

Key `just repro` output: the `prod-first-attempt` cases (the config eclair
actually uses for a payment's first attempt) print `NO ROUTE` for every
amount, while the `inspect k-shortest candidate paths` case prints the five
candidates and their full-amount fees.

### Manual: eclair side

Requires JDK 21 and a checkout of [ACINQ/eclair](https://github.com/ACINQ/eclair)
(tested at master `7fb9460`, 2026-07-08):

```bash
cp eclair/LexeGraphDebugSpec.scala <eclair>/eclair-core/src/test/scala/fr/acinq/eclair/router/
cd <eclair>
LEXE_GRAPH_CSV=<this-repo>/data/graph.csv \
  ./mvnw -pl eclair-core test -Dsuites='fr.acinq.eclair.router.LexeGraphDebugSpec'
```

### Manual: LDK side

Requires Rust (any recent stable). From the repo root:

```bash
# graph + scorer load
cargo run --manifest-path ldk/Cargo.toml -- stats

# baseline: LDK finds an in-budget route for the actual failed invoice,
# even with eclair's CLTV budget
cargo run --manifest-path ldk/Cargo.toml -- route-bolt12 data/bolt12_invoice.txt --max-cltv 576

# the Lexe LSP's channels, policies, and scorer liquidity estimates
cargo run --manifest-path ldk/Cargo.toml -- node 0314a77523d1dcbc5db56081edcbc24ab820b35e343a6c6769176de707c178d457

# all ACINQ -> peer -> Lexe 2-hop paths with eclair-style fee accounting
# (amount = invoice amount + blinded path fee)
cargo run --manifest-path ldk/Cargo.toml -- two-hop \
  03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f \
  0314a77523d1dcbc5db56081edcbc24ab820b35e343a6c6769176de707c178d457 \
  --amount-msat 350757124

# regenerate data/graph.csv from the raw dump
cargo run --manifest-path ldk/Cargo.toml -- export
```

### Correlating with ACINQ's logs

Payment hashes
`75b7eb0bc8ab9085e322721c1cc6ac2cd7d5bbbab28c415daed43aeec5543671`,
`b4d79901735c18d6e0ce3fdd8f0fdbe2b7b80a8114cc1dd195ba48423abe612e` (and 6
more), 2026-07-09 between 14:33 and 16:13 (payer's log time), each failed
upstream ~1 s after the HTLC set completed.
