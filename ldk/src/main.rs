//! Debug CLI for lexe-app/lexe-public#79: Phoenix -> Lexe payments failing
//! at ACINQ's trampoline node.
//!
//! Works off the serialized LDK `NetworkGraph` + `ProbabilisticScorer` dumps
//! in `data/` (taken from Lexe's LSP on 2026-07-09) and the invoices Phoenix
//! received from the Lexe recipient.
//!
//! Usage: `cargo run --manifest-path ldk/Cargo.toml -- <subcommand>` from the
//! repository root (default data paths are relative to the repo root).

use std::{fs, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use anyhow::{anyhow, Context};
use bech32::{primitives::decode::CheckedHrpstring, NoChecksum};
use bitcoin::secp256k1::PublicKey;
use clap::{Parser, Subcommand};
use lightning::{
    blinded_path::IntroductionNode,
    bolt11_invoice::Bolt11Invoice,
    offers::invoice::Bolt12Invoice,
    routing::{
        gossip::{ChannelUpdateInfo, NetworkGraph, NodeId},
        router::{find_route, PaymentParameters, Route, RouteParameters},
        scoring::{
            ProbabilisticScorer, ProbabilisticScoringDecayParameters,
            ProbabilisticScoringFeeParameters,
        },
    },
    util::{
        logger::{Level, Logger, Record},
        ser::ReadableArgs,
    },
};

/// ACINQ's trampoline node (Phoenix's LSP).
const ACINQ_NODE_ID: &str =
    "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";

/// Phoenix's only trampoline fee tier: 4 sat base + 0.4% proportional.
const TRAMPOLINE_FEE_BASE_MSAT: u64 = 4_000;
const TRAMPOLINE_FEE_PROP_MILLIONTHS: u64 = 4_000;

type GraphType = NetworkGraph<Arc<DebugLogger>>;
type ScorerType = ProbabilisticScorer<Arc<GraphType>, Arc<DebugLogger>>;

#[derive(Parser)]
struct Args {
    /// Path to a serialized LDK `NetworkGraph` dump.
    #[arg(long, default_value = "data/2026_07_09-network_graph")]
    graph: PathBuf,
    /// Path to a serialized LDK `ProbabilisticScorer` dump.
    #[arg(long, default_value = "data/2026_07_09-prob_scorer")]
    scorer: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print graph and scorer stats.
    Stats,
    /// Parse and print a BOLT11 invoice (arg or file path).
    Bolt11 { invoice: String },
    /// Parse and print BOLT12 invoice(s) (arg or file path, one per line).
    Bolt12 { invoice: String },
    /// Print a node's summary, channels, policies, and scorer estimates.
    Node { node_id: String },
    /// Print direct channels between two nodes.
    Direct { node_a: String, node_b: String },
    /// Enumerate 2-hop paths `src -> peer -> dst` with per-edge fees, like
    /// eclair's trampoline relay sees them (`includeLocalChannelCost=true`:
    /// the src's own first-hop fee counts against the fee budget).
    TwoHop {
        src: String,
        dst: String,
        /// Amount to deliver to `dst`, msat (for fee computation).
        #[arg(long)]
        amount_msat: u64,
        /// Print only paths with total fee <= this, msat.
        #[arg(long)]
        max_fee_msat: Option<u64>,
    },
    /// Export the graph as CSV for consumption by other implementations
    /// (e.g. an eclair test). One row per direction:
    /// `scid,from,to,capacity_sat,enabled,fee_base_msat,fee_prop_millionths,cltv_delta,htlc_min_msat,htlc_max_msat,last_update`
    Export {
        /// Output path.
        #[arg(long, default_value = "data/graph.csv")]
        out: PathBuf,
    },
    /// Find a route to a BOLT12 invoice's blinded paths.
    RouteBolt12 {
        /// BOLT12 invoice (arg or file path; first line if multiple).
        invoice: String,
        /// Amount to send, msat. Default: the invoice amount.
        #[arg(long)]
        amount_msat: Option<u64>,
        /// Max total routing fee, msat. Defaults to Phoenix's trampoline
        /// budget for the amount. Pass `--no-fee-limit` for none.
        #[arg(long)]
        max_fee_msat: Option<u64>,
        #[arg(long)]
        no_fee_limit: bool,
        /// Max total CLTV delta. Default: LDK's 1008. Phoenix's trampoline
        /// tier gives eclair a budget of 576.
        #[arg(long)]
        max_cltv: Option<u32>,
        /// Source node. Default: ACINQ's trampoline node.
        #[arg(long)]
        from: Option<String>,
    },
    /// Find a route to a BOLT11 invoice's payee via its route hints.
    RouteBolt11 {
        /// BOLT11 invoice (arg or file path).
        invoice: String,
        /// Amount to send, msat. Default: the invoice amount.
        #[arg(long)]
        amount_msat: Option<u64>,
        /// Max total routing fee, msat. Defaults to Phoenix's trampoline
        /// budget for the amount. Pass `--no-fee-limit` for none.
        #[arg(long)]
        max_fee_msat: Option<u64>,
        #[arg(long)]
        no_fee_limit: bool,
        /// Max total CLTV delta. Default: LDK's 1008. Phoenix's trampoline
        /// tier gives eclair a budget of 576.
        #[arg(long)]
        max_cltv: Option<u32>,
        /// Source node. Default: ACINQ's trampoline node.
        #[arg(long)]
        from: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let ctx = Ctx {
        graph_path: args.graph,
        scorer_path: args.scorer,
        logger: Arc::new(DebugLogger),
    };

    match args.cmd {
        Cmd::Stats => ctx.stats(),
        Cmd::Bolt11 { invoice } => ctx.bolt11(&invoice),
        Cmd::Bolt12 { invoice } => ctx.bolt12(&invoice),
        Cmd::Node { node_id } => ctx.node(&node_id),
        Cmd::Direct { node_a, node_b } => ctx.direct(&node_a, &node_b),
        Cmd::TwoHop {
            src,
            dst,
            amount_msat,
            max_fee_msat,
        } => ctx.two_hop(&src, &dst, amount_msat, max_fee_msat),
        Cmd::Export { out } => ctx.export(&out),
        Cmd::RouteBolt12 {
            invoice,
            amount_msat,
            max_fee_msat,
            no_fee_limit,
            max_cltv,
            from,
        } => ctx.route_bolt12(
            &invoice,
            amount_msat,
            fee_limit(max_fee_msat, no_fee_limit),
            max_cltv,
            from.as_deref(),
        ),
        Cmd::RouteBolt11 {
            invoice,
            amount_msat,
            max_fee_msat,
            no_fee_limit,
            max_cltv,
            from,
        } => ctx.route_bolt11(
            &invoice,
            amount_msat,
            fee_limit(max_fee_msat, no_fee_limit),
            max_cltv,
            from.as_deref(),
        ),
    }
}

/// `None` = default to the trampoline budget; `Some(None)` = no limit.
fn fee_limit(
    max_fee_msat: Option<u64>,
    no_fee_limit: bool,
) -> Option<Option<u64>> {
    if no_fee_limit {
        Some(None)
    } else {
        max_fee_msat.map(Some)
    }
}

/// Phoenix's trampoline fee budget for a given amount.
fn trampoline_budget_msat(amount_msat: u64) -> u64 {
    TRAMPOLINE_FEE_BASE_MSAT
        + amount_msat * TRAMPOLINE_FEE_PROP_MILLIONTHS / 1_000_000
}

/// Reads `s` as a file path if one exists, else treats `s` as the value.
fn arg_or_file(s: &str) -> anyhow::Result<String> {
    let path = PathBuf::from(s);
    if path.exists() {
        fs::read_to_string(&path).with_context(|| format!("{path:?}"))
    } else {
        Ok(s.to_owned())
    }
}

struct Ctx {
    graph_path: PathBuf,
    scorer_path: PathBuf,
    logger: Arc<DebugLogger>,
}

impl Ctx {
    fn load_graph(&self) -> anyhow::Result<Arc<GraphType>> {
        let bytes = fs::read(&self.graph_path)
            .with_context(|| format!("{:?}", self.graph_path))?;
        let graph =
            GraphType::read(&mut bytes.as_slice(), self.logger.clone())
                .map_err(|e| anyhow!("read graph: {e:?}"))?;
        Ok(Arc::new(graph))
    }

    fn load_scorer(
        &self,
        graph: &Arc<GraphType>,
    ) -> anyhow::Result<ScorerType> {
        // Decay params only affect in-memory decay, not deserialization; use
        // the LSP's values.
        let decay_params = ProbabilisticScoringDecayParameters {
            historical_no_updates_half_life: Duration::from_secs(
                30 * 24 * 60 * 60,
            ),
            liquidity_offset_half_life: Duration::from_secs(
                14 * 24 * 60 * 60,
            ),
        };
        let bytes = fs::read(&self.scorer_path)
            .with_context(|| format!("{:?}", self.scorer_path))?;
        ScorerType::read(
            &mut bytes.as_slice(),
            (decay_params, graph.clone(), self.logger.clone()),
        )
        .map_err(|e| anyhow!("read scorer: {e:?}"))
    }

    // --- Subcommands --- //

    fn stats(&self) -> anyhow::Result<()> {
        let graph = self.load_graph()?;
        let ro = graph.read_only();
        println!(
            "network graph: {} nodes, {} channels",
            ro.nodes().len(),
            ro.channels().len(),
        );
        drop(ro);
        let scorer = self.load_scorer(&graph)?;
        let _ = scorer;
        println!("scorer: loaded ok");
        Ok(())
    }

    fn bolt11(&self, invoice: &str) -> anyhow::Result<()> {
        let s = arg_or_file(invoice)?;
        let invoice = parse_bolt11(s.trim())?;
        println!("payee: {}", invoice.recover_payee_pub_key());
        println!("amount: {:?} msat", invoice.amount_milli_satoshis());
        println!(
            "min_final_cltv: {}",
            invoice.min_final_cltv_expiry_delta()
        );
        println!("expiry: {:?}", invoice.expiry_time());
        for hint in invoice.route_hints() {
            for hop in &hint.0 {
                println!(
                    "route hint hop: src={} scid={} fees={{base: {} msat, \
                     prop: {} ppm}} cltv_delta={} htlc_min={:?} \
                     htlc_max={:?}",
                    hop.src_node_id,
                    hop.short_channel_id,
                    hop.fees.base_msat,
                    hop.fees.proportional_millionths,
                    hop.cltv_expiry_delta,
                    hop.htlc_minimum_msat,
                    hop.htlc_maximum_msat,
                );
            }
        }
        Ok(())
    }

    fn bolt12(&self, invoice: &str) -> anyhow::Result<()> {
        let s = arg_or_file(invoice)?;
        for line in s.lines().filter(|l| !l.trim().is_empty()) {
            let invoice = parse_bolt12(line.trim())?;
            print_bolt12(&invoice);
        }
        Ok(())
    }

    fn node(&self, node_id: &str) -> anyhow::Result<()> {
        let graph = self.load_graph()?;
        let scorer = self.load_scorer(&graph)?;
        let node_id = parse_node_id(node_id)?;
        print_node_summary(&graph, &node_id);
        print_node_channels(&graph, &scorer, &node_id);
        Ok(())
    }

    fn direct(&self, node_a: &str, node_b: &str) -> anyhow::Result<()> {
        let graph = self.load_graph()?;
        let a = parse_node_id(node_a)?;
        let b = parse_node_id(node_b)?;
        let ro = graph.read_only();
        let mut count = 0;
        for (scid, chan) in ro.channels().unordered_iter() {
            if (chan.node_one == a && chan.node_two == b)
                || (chan.node_one == b && chan.node_two == a)
            {
                count += 1;
                println!(
                    "scid={scid} cap={:?} sat one_to_two={} two_to_one={}",
                    chan.capacity_sats,
                    policy_str(&chan.one_to_two),
                    policy_str(&chan.two_to_one),
                );
            }
        }
        println!("{count} direct channels");
        Ok(())
    }

    fn two_hop(
        &self,
        src: &str,
        dst: &str,
        amount_msat: u64,
        max_fee_msat: Option<u64>,
    ) -> anyhow::Result<()> {
        let graph = self.load_graph()?;
        let scorer = self.load_scorer(&graph)?;
        let src = parse_node_id(src)?;
        let dst = parse_node_id(dst)?;
        let ro = graph.read_only();
        let dst_info = ro.node(&dst).context("dst not in graph")?;

        struct Row {
            peer: NodeId,
            scid1: u64,
            scid2: u64,
            fee_first_msat: u64,
            fee_last_msat: u64,
            enabled: bool,
            liq1: Option<(u64, u64)>,
            liq2: Option<(u64, u64)>,
        }
        let fee_for = |p: &ChannelUpdateInfo, amt: u64| {
            p.fees.base_msat as u64
                + amt * p.fees.proportional_millionths as u64 / 1_000_000
        };

        let mut rows = Vec::new();
        // Last hop: peer -> dst, policy set by peer.
        for scid2 in &dst_info.channels {
            let Some(chan2) = ro.channel(*scid2) else {
                continue;
            };
            let (peer, last_policy) = if chan2.node_one == dst {
                (chan2.node_two, &chan2.two_to_one)
            } else {
                (chan2.node_one, &chan2.one_to_two)
            };
            let Some(last_policy) = last_policy else {
                continue;
            };
            // First hop: src -> peer, policy set by src (eclair counts this
            // against the fee budget via includeLocalChannelCost=true).
            let Some(peer_info) = ro.node(&peer) else {
                continue;
            };
            for scid1 in &peer_info.channels {
                let Some(chan1) = ro.channel(*scid1) else {
                    continue;
                };
                let (other, first_policy) = if chan1.node_one == peer {
                    (chan1.node_two, &chan1.two_to_one)
                } else {
                    (chan1.node_one, &chan1.one_to_two)
                };
                if other != src {
                    continue;
                }
                let Some(first_policy) = first_policy else {
                    continue;
                };
                let fee_last_msat = fee_for(last_policy, amount_msat);
                let fee_first_msat =
                    fee_for(first_policy, amount_msat + fee_last_msat);
                rows.push(Row {
                    peer,
                    scid1: *scid1,
                    scid2: *scid2,
                    fee_first_msat,
                    fee_last_msat,
                    enabled: first_policy.enabled && last_policy.enabled,
                    liq1: scorer
                        .estimated_channel_liquidity_range(*scid1, &peer),
                    liq2: scorer
                        .estimated_channel_liquidity_range(*scid2, &dst),
                });
            }
        }
        drop(ro);

        rows.sort_by_key(|r| r.fee_first_msat + r.fee_last_msat);
        println!(
            "2-hop paths src -> peer -> dst for {amount_msat} msat \
             (total = src's own fee + peer's fee):"
        );
        for r in rows {
            let total = r.fee_first_msat + r.fee_last_msat;
            if let Some(max) = max_fee_msat {
                if total > max {
                    continue;
                }
            }
            println!(
                "total={total} msat first_hop_fee={} last_hop_fee={} \
                 enabled={} peer={:?} ({})\n  scid1={} liq_toward_peer={:?}\n  \
                 scid2={} liq_toward_dst={:?}",
                r.fee_first_msat,
                r.fee_last_msat,
                r.enabled,
                node_alias(&graph, &r.peer),
                r.peer,
                r.scid1,
                r.liq1,
                r.scid2,
                r.liq2,
            );
        }
        Ok(())
    }

    fn export(&self, out: &PathBuf) -> anyhow::Result<()> {
        use std::io::Write;
        let graph = self.load_graph()?;
        let ro = graph.read_only();
        let mut w = std::io::BufWriter::new(fs::File::create(out)?);
        writeln!(
            w,
            "scid,from,to,capacity_sat,enabled,fee_base_msat,\
             fee_prop_millionths,cltv_delta,htlc_min_msat,htlc_max_msat,\
             last_update"
        )?;
        let mut n = 0u64;
        for (scid, chan) in ro.channels().unordered_iter() {
            let mut write_dir = |from: &NodeId,
                                 to: &NodeId,
                                 policy: &Option<ChannelUpdateInfo>|
             -> anyhow::Result<()> {
                let Some(p) = policy else {
                    return Ok(());
                };
                writeln!(
                    w,
                    "{scid},{from},{to},{},{},{},{},{},{},{},{}",
                    chan.capacity_sats.unwrap_or(0),
                    p.enabled,
                    p.fees.base_msat,
                    p.fees.proportional_millionths,
                    p.cltv_expiry_delta,
                    p.htlc_minimum_msat,
                    p.htlc_maximum_msat,
                    p.last_update,
                )?;
                Ok(())
            };
            write_dir(&chan.node_one, &chan.node_two, &chan.one_to_two)?;
            write_dir(&chan.node_two, &chan.node_one, &chan.two_to_one)?;
            n += 1;
        }
        println!("exported {n} channels to {}", out.display());
        Ok(())
    }

    fn route_bolt12(
        &self,
        invoice: &str,
        amount_msat: Option<u64>,
        fee_limit: Option<Option<u64>>,
        max_cltv: Option<u32>,
        from: Option<&str>,
    ) -> anyhow::Result<()> {
        let s = arg_or_file(invoice)?;
        let line = s.lines().next().context("empty invoice input")?;
        let invoice = parse_bolt12(line.trim())?;
        print_bolt12(&invoice);

        let amount_msat = amount_msat.unwrap_or_else(|| invoice.amount_msats());
        let mut payment_params =
            PaymentParameters::from_bolt12_invoice(&invoice);
        if let Some(max_cltv) = max_cltv {
            payment_params =
                payment_params.with_max_total_cltv_expiry_delta(max_cltv);
        }
        self.route(payment_params, amount_msat, fee_limit, from)
    }

    fn route_bolt11(
        &self,
        invoice: &str,
        amount_msat: Option<u64>,
        fee_limit: Option<Option<u64>>,
        max_cltv: Option<u32>,
        from: Option<&str>,
    ) -> anyhow::Result<()> {
        let s = arg_or_file(invoice)?;
        let invoice = parse_bolt11(s.trim())?;
        let payee = invoice.recover_payee_pub_key();
        println!("payee: {payee}");

        let amount_msat = amount_msat
            .or(invoice.amount_milli_satoshis())
            .context("no amount; pass --amount-msat")?;
        let mut payment_params = PaymentParameters::from_node_id(
            payee,
            invoice.min_final_cltv_expiry_delta() as u32,
        )
        .with_route_hints(invoice.route_hints())
        .map_err(|()| anyhow!("with_route_hints"))?;
        if let Some(max_cltv) = max_cltv {
            payment_params =
                payment_params.with_max_total_cltv_expiry_delta(max_cltv);
        }
        self.route(payment_params, amount_msat, fee_limit, from)
    }

    fn route(
        &self,
        payment_params: PaymentParameters,
        amount_msat: u64,
        fee_limit: Option<Option<u64>>,
        from: Option<&str>,
    ) -> anyhow::Result<()> {
        let graph = self.load_graph()?;
        let scorer = self.load_scorer(&graph)?;
        let from_pk = match from {
            Some(s) => PublicKey::from_str(s)
                .map_err(|e| anyhow!("bad --from: {e}"))?,
            None => PublicKey::from_str(ACINQ_NODE_ID).unwrap(),
        };
        let max_fee_msat = fee_limit
            .unwrap_or_else(|| Some(trampoline_budget_msat(amount_msat)));
        println!(
            "routing: from={from_pk} amount={amount_msat} msat \
             max_fee={max_fee_msat:?} msat"
        );

        let mut route_params = RouteParameters::from_payment_params_and_value(
            payment_params,
            amount_msat,
        );
        route_params.max_total_routing_fee_msat = max_fee_msat;

        let score_params = ProbabilisticScoringFeeParameters::default();
        let route = find_route(
            &from_pk,
            &route_params,
            &graph,
            None,
            self.logger.clone(),
            &scorer,
            &score_params,
            &[42u8; 32],
        );
        print_route_result(&graph, &scorer, &route);
        Ok(())
    }
}

// --- Parsing helpers --- //

fn parse_node_id(s: &str) -> anyhow::Result<NodeId> {
    let pk =
        PublicKey::from_str(s).map_err(|e| anyhow!("bad node id: {e}"))?;
    Ok(NodeId::from_pubkey(&pk))
}

fn parse_bolt11(s: &str) -> anyhow::Result<Bolt11Invoice> {
    let s = s.trim().trim_start_matches("lightning:");
    Bolt11Invoice::from_str(s).map_err(|e| anyhow!("parse bolt11: {e:?}"))
}

/// Parses a bech32-encoded (no checksum, `lni` hrp) BOLT12 invoice.
/// LDK doesn't expose this since BOLT12 invoices normally travel in onion
/// messages, but lightning-kmp logs them bech32-encoded.
fn parse_bolt12(s: &str) -> anyhow::Result<Bolt12Invoice> {
    let parsed = CheckedHrpstring::new::<NoChecksum>(s)
        .map_err(|e| anyhow!("bech32: {e:?}"))?;
    let hrp = parsed.hrp();
    anyhow::ensure!(
        hrp.lowercase_char_iter().eq("lni".chars()),
        "bad hrp: {hrp}"
    );
    let data = parsed.byte_iter().collect::<Vec<u8>>();
    Bolt12Invoice::try_from(data).map_err(|e| anyhow!("bolt12: {e:?}"))
}

// --- Printing helpers --- //

fn print_bolt12(invoice: &Bolt12Invoice) {
    println!(
        "bolt12 invoice: amount={} msat payment_hash={} signing_pk={} \
         created_at={:?}",
        invoice.amount_msats(),
        invoice.payment_hash(),
        invoice.signing_pubkey(),
        invoice.created_at(),
    );
    for path in invoice.payment_paths() {
        let intro = match path.introduction_node() {
            IntroductionNode::NodeId(pk) => format!("node_id={pk}"),
            IntroductionNode::DirectedShortChannelId(d, scid) =>
                format!("directed_scid={scid} ({d:?})"),
        };
        let payinfo = &path.payinfo;
        println!(
            "  path: intro={intro} blinded_hops={} payinfo={{fee_base: {} \
             msat, fee_prop: {} ppm, cltv_delta: {}, htlc_min: {}, htlc_max: \
             {}}}",
            path.blinded_hops().len(),
            payinfo.fee_base_msat,
            payinfo.fee_proportional_millionths,
            payinfo.cltv_expiry_delta,
            payinfo.htlc_minimum_msat,
            payinfo.htlc_maximum_msat,
        );
    }
}

fn node_alias(graph: &GraphType, node_id: &NodeId) -> String {
    graph
        .read_only()
        .node(node_id)
        .and_then(|info| info.announcement_info.as_ref().map(|a| a.alias()))
        .map(|a| a.to_string())
        .unwrap_or_else(|| "<unknown>".to_owned())
}

fn policy_str(policy: &Option<ChannelUpdateInfo>) -> String {
    match policy {
        Some(p) => format!(
            "{{enabled={} fees={}+{}ppm htlc_max={:?} cltv_delta={}}}",
            p.enabled,
            p.fees.base_msat,
            p.fees.proportional_millionths,
            p.htlc_maximum_msat,
            p.cltv_expiry_delta,
        ),
        None => "<none>".to_owned(),
    }
}

fn print_node_summary(graph: &GraphType, node_id: &NodeId) {
    let ro = graph.read_only();
    match ro.node(node_id) {
        Some(info) => {
            let alias = info
                .announcement_info
                .as_ref()
                .map(|a| a.alias().to_string())
                .unwrap_or_else(|| "<no announcement>".to_owned());
            println!(
                "node {node_id}: alias={alias:?} public_channels={}",
                info.channels.len()
            );
        }
        None => println!("node {node_id}: NOT IN GRAPH"),
    }
}

/// Prints all of a node's public channels with per-direction policies and
/// the scorer's liquidity estimates.
fn print_node_channels(
    graph: &GraphType,
    scorer: &ScorerType,
    node_id: &NodeId,
) {
    let ro = graph.read_only();
    let Some(info) = ro.node(node_id) else {
        return;
    };
    for scid in &info.channels {
        let Some(chan) = ro.channel(*scid) else {
            continue;
        };
        let (peer, inbound_policy, outbound_policy) =
            if chan.node_one == *node_id {
                (&chan.node_two, &chan.two_to_one, &chan.one_to_two)
            } else {
                (&chan.node_one, &chan.one_to_two, &chan.two_to_one)
            };
        let peer_alias = node_alias(graph, peer);
        let cap = chan
            .capacity_sats
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_owned());
        // Scorer's estimated liquidity toward this node (i.e. inbound).
        let liq = scorer
            .estimated_channel_liquidity_range(*scid, node_id)
            .map(|(min, max)| format!("[{min}, {max}]"))
            .unwrap_or_else(|| "<none>".to_owned());
        println!(
            "scid={scid} peer={peer_alias:?} cap={cap} sat\n  inbound:  \
             {}\n  outbound: {}\n  est. inbound liquidity: {liq} msat",
            policy_str(inbound_policy),
            policy_str(outbound_policy),
        );
    }
}

fn print_route_result(
    graph: &GraphType,
    scorer: &ScorerType,
    route: &Result<Route, &'static str>,
) {
    match route {
        Err(e) => println!("NO ROUTE: {e}"),
        Ok(route) => {
            println!(
                "found route: total_fee={} msat total_amount={} msat \
                 paths={}",
                route.get_total_fees(),
                route.get_total_amount(),
                route.paths.len(),
            );
            for path in &route.paths {
                for hop in &path.hops {
                    let alias =
                        node_alias(graph, &NodeId::from_pubkey(&hop.pubkey));
                    let liq = scorer
                        .estimated_channel_liquidity_range(
                            hop.short_channel_id,
                            &NodeId::from_pubkey(&hop.pubkey),
                        )
                        .map(|(min, max)| format!("[{min}, {max}]"))
                        .unwrap_or_else(|| "<none>".to_owned());
                    println!(
                        "  hop: {} ({alias:?}) scid={} fee={} msat \
                         cltv_delta={} est_liq={liq}",
                        hop.pubkey,
                        hop.short_channel_id,
                        hop.fee_msat,
                        hop.cltv_expiry_delta,
                    );
                }
                if let Some(tail) = &path.blinded_tail {
                    println!(
                        "  blinded tail: {} hops, final_value={} msat",
                        tail.hops.len(),
                        tail.final_value_msat,
                    );
                }
            }
        }
    }
}

/// Prints LDK logs to stderr. Set `LOG_TRACE=1` for gossip/trace spam.
struct DebugLogger;

impl Logger for DebugLogger {
    fn log(&self, record: Record<'_>) {
        let trace = std::env::var("LOG_TRACE").is_ok();
        if record.level >= Level::Debug || trace {
            eprintln!("{}: {}", record.level, record.args);
        }
    }
}
