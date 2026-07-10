package fr.acinq.eclair.router

import fr.acinq.bitcoin.scalacompat.Crypto.PublicKey
import fr.acinq.bitcoin.scalacompat.{Block, Satoshi}
import fr.acinq.eclair.payment.Invoice
import fr.acinq.eclair.payment.relay.Relayer.RelayFees
import fr.acinq.eclair.router.Graph.GraphStructure.{DirectedGraph, GraphEdge}
import fr.acinq.eclair.router.Graph.HeuristicsConstants
import fr.acinq.eclair.router.RouteCalculation._
import fr.acinq.eclair.router.Router._
import fr.acinq.eclair.transactions.Transactions
import fr.acinq.eclair.wire.protocol._
import fr.acinq.eclair.{BlockHeight, CltvExpiryDelta, MilliSatoshi, MilliSatoshiLong, RealShortChannelId, ShortChannelId, TimestampSecond, ToMilliSatoshiConversion, randomKey}
import org.scalatest.funsuite.AnyFunSuite
import scodec.bits.ByteVector

import scala.concurrent.duration.DurationInt
import scala.util.{Failure, Success}

/**
 * Debug harness for lexe-app/lexe-public#79: replicate what ACINQ's
 * trampoline node (eclair) would compute when routing Phoenix -> Lexe
 * payments, using a graph exported from Lexe's LSP
 * (`graph-debug export` -> graph.csv).
 *
 * Numbers below come from the actual failed payments on 2026-07-09
 * (Phoenix logs + decoded Lexe BOLT12 invoices):
 * - amount 349,708,000 msat
 * - Phoenix trampoline fee budget: 4 sat + 0.4% = 1,402,832 msat
 * - Phoenix trampoline cltv budget: 576
 * - Lexe blinded path: intro = Lexe LSP, fee 0+3000ppm, cltv delta 114,
 *   htlc max 99,700,897,309 msat
 *
 * Run: LEXE_GRAPH_CSV=<path> ./mvnw -pl eclair-core test -Dsuites='fr.acinq.eclair.router.LexeGraphDebugSpec'
 */
class LexeGraphDebugSpec extends AnyFunSuite {

  val acinq = PublicKey(ByteVector.fromValidHex("03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f"))
  val lexeLsp = PublicKey(ByteVector.fromValidHex("0314a77523d1dcbc5db56081edcbc24ab820b35e343a6c6769176de707c178d457"))

  val amountMsat = 349_708_000L
  val maxFeeMsat = 1_402_832L
  val maxCltv = 576
  val blockHeight = BlockHeight(957_338)

  // eclair reference.conf `path-finding.default` heuristics.
  val defaultHeuristics = HeuristicsConstants(
    lockedFundsRisk = 1e-8,
    failureFees = RelayFees(2_000 msat, 500),
    hopFees = RelayFees(500 msat, 200),
    useLogProbability = false,
    usePastRelaysData = false,
  )

  // Fee-only weights, for comparison.
  val feeOnlyHeuristics = HeuristicsConstants(
    lockedFundsRisk = 0.0,
    failureFees = RelayFees(0 msat, 0),
    hopFees = RelayFees(0 msat, 0),
    useLogProbability = false,
    usePastRelaysData = false,
  )

  /** `NodeRelay.computeRouteParams` boundaries with reference.conf defaults elsewhere. */
  def routeParams(heuristics: HeuristicsConstants, randomize: Boolean): RouteParams = RouteParams(
    randomize = randomize,
    boundaries = SearchBoundaries(
      maxFeeFlat = MilliSatoshi(maxFeeMsat),
      maxFeeProportional = 0, // NodeRelay disables the percent-based max fee
      maxRouteLength = 6,
      maxCltv = CltvExpiryDelta(maxCltv),
    ),
    heuristics = heuristics,
    mpp = MultiPartParams(15_000_000 msat, 5, MultiPartParams.Randomize),
    experimentName = "lexe-debug",
    includeLocalChannelCost = true,
  )

  /** Loads a `graph-debug export` CSV into eclair's graph. */
  def loadGraph(): DirectedGraph = {
    val path = sys.env.getOrElse("LEXE_GRAPH_CSV", "graph.csv")
    val src = scala.io.Source.fromFile(path)
    try {
      val edges = src.getLines().drop(1).flatMap { line =>
        // scid,from,to,capacity_sat,enabled,fee_base_msat,fee_prop_millionths,
        // cltv_delta,htlc_min_msat,htlc_max_msat,last_update
        val f = line.split(',')
        val scid = RealShortChannelId(f(0).toLong)
        val from = PublicKey(ByteVector.fromValidHex(f(1)))
        val to = PublicKey(ByteVector.fromValidHex(f(2)))
        val enabled = f(4).toBoolean
        if (!enabled) {
          // eclair removes disabled edges from its graph.
          None
        } else {
          val update = ChannelUpdate(
            signature = Transactions.PlaceHolderSig,
            chainHash = Block.LivenetGenesisBlock.hash,
            shortChannelId = scid,
            timestamp = TimestampSecond(f(10).toLong),
            messageFlags = ChannelUpdate.MessageFlags(dontForward = false),
            channelFlags = ChannelUpdate.ChannelFlags(isEnabled = true, isNode1 = Announcements.isNode1(from, to)),
            cltvExpiryDelta = CltvExpiryDelta(f(7).toInt),
            htlcMinimumMsat = MilliSatoshi(f(8).toLong),
            feeBaseMsat = MilliSatoshi(f(5).toLong),
            feeProportionalMillionths = f(6).toLong,
            htlcMaximumMsat = MilliSatoshi(f(9).toLong),
          )
          val desc = ChannelDesc(scid, from, to)
          Some(GraphEdge(desc, HopRelayParams.FromAnnouncement(update), Satoshi(f(3).toLong), balance_opt = None))
        }
      }.toSeq
      println(s"loaded ${edges.size} enabled directed edges")
      DirectedGraph(edges)
    } finally {
      src.close()
    }
  }

  /** The Lexe blinded path as a graph edge, like `computeTarget` builds it. */
  def blindedEdge(recipient: PublicKey): GraphEdge = {
    val extraEdge = Invoice.ExtraEdge(
      sourceNodeId = lexeLsp,
      targetNodeId = recipient,
      shortChannelId = ShortChannelId.generateLocalAlias(),
      feeBase = 0 msat,
      feeProportionalMillionths = 3000,
      cltvExpiryDelta = CltvExpiryDelta(114),
      htlcMinimum = 1 msat,
      htlcMaximum_opt = Some(MilliSatoshi(99_700_897_309L)),
    )
    GraphEdge(extraEdge).copy(balance_opt = extraEdge.htlcMaximum_opt)
  }

  def printResult(label: String, result: scala.util.Try[Seq[Route]], amount: MilliSatoshi): Unit = {
    result match {
      case Success(routes) =>
        println(s"[$label] found ${routes.size} route(s)")
        routes.foreach { route =>
          val fee = route.channelFee(includeLocalChannelCost = true)
          println(s"  route: amount=${route.amount} fee=$fee")
          route.hops.foreach { hop =>
            println(s"    hop: ${hop.nodeId} -> ${hop.nextNodeId} scid=${hop.shortChannelId} fee=${hop.fee(amount)}")
          }
        }
      case Failure(t) =>
        println(s"[$label] NO ROUTE: $t")
    }
  }

  test("route ACINQ -> Lexe blinded path, single-part") {
    val g = GraphWithBalanceEstimates(loadGraph(), 1 day)
    val recipient = randomKey().publicKey
    val edge = blindedEdge(recipient)
    val amount = MilliSatoshi(amountMsat)
    val maxFee = MilliSatoshi(maxFeeMsat)

    for ((heuristics, hLabel) <- Seq((defaultHeuristics, "default-heuristics"), (feeOnlyHeuristics, "fee-only"))) {
      for (randomize <- Seq(false, true)) {
        val numRoutes = if (randomize) DEFAULT_ROUTES_COUNT else 1
        val result = findRoute(g, acinq, recipient, amount, maxFee, numRoutes = numRoutes,
          extraEdges = Set(edge), routeParams = routeParams(heuristics, randomize), currentBlockHeight = blockHeight)
        printResult(s"single-part $hLabel randomize=$randomize", result, amount)
      }
    }
  }

  test("route ACINQ -> Lexe blinded path, multi-part") {
    val g = GraphWithBalanceEstimates(loadGraph(), 1 day)
    val recipient = randomKey().publicKey
    val edge = blindedEdge(recipient)
    val amount = MilliSatoshi(amountMsat)
    val maxFee = MilliSatoshi(maxFeeMsat)

    for ((heuristics, hLabel) <- Seq((defaultHeuristics, "default-heuristics"), (feeOnlyHeuristics, "fee-only"))) {
      val result = findMultiPartRoute(g, acinq, recipient, amount, maxFee,
        extraEdges = Set(edge), routeParams = routeParams(heuristics, randomize = true), currentBlockHeight = blockHeight)
      printResult(s"multi-part $hLabel", result, amount)
    }
  }

  test("production first attempt: randomize=false, FullCapacity splitting") {
    // MultiPartPaymentLifecycle overrides the route params on the first
    // attempt: `copy(randomize = false, splittingStrategy = FullCapacity)`.
    // On PaymentRouteNotFound with nothing ignored yet, the payment fails
    // immediately (no retry). So the production outcome is a single
    // deterministic route calculation.
    val baseEdges = loadGraph().edgeSet().toSeq
    for (balanceFraction <- Seq(Option.empty[Double], Some(1.0), Some(0.5), Some(0.1))) {
      val edges = balanceFraction match {
        case None => baseEdges
        case Some(f) => baseEdges.map {
          case e if e.desc.a == acinq => e.copy(balance_opt = Some(e.capacity.toMilliSatoshi * f))
          case e => e
        }
      }
      val g = GraphWithBalanceEstimates(DirectedGraph(edges), 1 day)
      for (amountMsat <- Seq(349_708_000L, 343_378_000L, 158_239_000L, 79_119_000L)) {
        val recipient = randomKey().publicKey
        val edge = blindedEdge(recipient)
        val amount = MilliSatoshi(amountMsat)
        val maxFee = MilliSatoshi(4_000 + amountMsat * 4_000 / 1_000_000)
        val params = routeParams(defaultHeuristics, randomize = false)
          .copy(mpp = MultiPartParams(15_000_000 msat, 5, MultiPartParams.FullCapacity))
        val result = findMultiPartRoute(g, acinq, recipient, amount, maxFee,
          extraEdges = Set(edge), routeParams = params, currentBlockHeight = blockHeight)
        printResult(s"prod-first-attempt balances=$balanceFraction amount=$amountMsat maxFee=$maxFee", result, amount)
      }
    }
  }

  test("inspect k-shortest candidate paths for the MPP part amount") {
    // findMultiPartRouteInternal runs the path search with
    // amount = max(minPartAmount, total/maxParts) = 69,941,600 msat and
    // numRoutes = maxParts = 5, then splits the total across those paths and
    // validates the *total* fee. Show which paths the search picks for the
    // part amount, and what they cost at the full amount.
    val g = GraphWithBalanceEstimates(loadGraph(), 1 day)
    val recipient = randomKey().publicKey
    val edge = blindedEdge(recipient)
    val partAmount = MilliSatoshi(69_941_600L)
    val fullAmount = MilliSatoshi(amountMsat)
    val maxFee = MilliSatoshi(maxFeeMsat)
    val params = routeParams(defaultHeuristics, randomize = false)
    val result = findRoute(g, acinq, recipient, partAmount, maxFee, numRoutes = 5,
      extraEdges = Set(edge), routeParams = params, currentBlockHeight = blockHeight)
    result match {
      case Success(routes) =>
        routes.foreach { route =>
          // What this path costs at the full amount, as validateMultiPartRoute would see it.
          val fullFee = route.copy(amount = fullAmount).channelFee(includeLocalChannelCost = true)
          val partFee = route.channelFee(includeLocalChannelCost = true)
          println(s"candidate path: partFee=$partFee fullAmountFee=$fullFee (budget=$maxFee)")
          route.hops.foreach { hop =>
            println(s"  hop: ${hop.nodeId.toString.take(16)} -> ${hop.nextNodeId.toString.take(16)} scid=${hop.shortChannelId} base=${hop.params.relayFees.feeBase} prop=${hop.params.relayFees.feeProportionalMillionths}")
          }
        }
      case Failure(t) => println(s"candidate path search failed: $t")
    }
  }

  test("multi-part default heuristics, 100 iterations") {
    val g = GraphWithBalanceEstimates(loadGraph(), 1 day)
    val amount = MilliSatoshi(amountMsat)
    val maxFee = MilliSatoshi(maxFeeMsat)
    var successes = 0
    var failures = 0
    var totalFees = List.empty[MilliSatoshi]
    for (_ <- 1 to 100) {
      val recipient = randomKey().publicKey
      val edge = blindedEdge(recipient)
      findMultiPartRoute(g, acinq, recipient, amount, maxFee,
        extraEdges = Set(edge), routeParams = routeParams(defaultHeuristics, randomize = true), currentBlockHeight = blockHeight) match {
        case Success(routes) =>
          successes += 1
          totalFees = routes.map(_.channelFee(includeLocalChannelCost = true)).sum :: totalFees
        case Failure(_) => failures += 1
      }
    }
    println(s"[mpp-100] successes=$successes failures=$failures")
    if (totalFees.nonEmpty) {
      println(s"[mpp-100] fees min=${totalFees.min} max=${totalFees.max}")
    }
  }

  test("multi-part, ACINQ balances depleted on cheap first hops") {
    // Simulate ACINQ having little outgoing liquidity: cap the balance of
    // every ACINQ-outgoing edge at various fractions of capacity, and see
    // when routing starts failing.
    val baseEdges = {
      val g = loadGraph()
      g.edgeSet().toSeq
    }
    val amount = MilliSatoshi(amountMsat)
    val maxFee = MilliSatoshi(maxFeeMsat)
    for (fraction <- Seq(1.0, 0.5, 0.1, 0.02, 0.002)) {
      val edges = baseEdges.map {
        case e if e.desc.a == acinq =>
          e.copy(balance_opt = Some(e.capacity.toMilliSatoshi * fraction))
        case e => e
      }
      val g = GraphWithBalanceEstimates(DirectedGraph(edges), 1 day)
      var successes = 0
      var failures = 0
      for (_ <- 1 to 20) {
        val recipient = randomKey().publicKey
        val edge = blindedEdge(recipient)
        findMultiPartRoute(g, acinq, recipient, amount, maxFee,
          extraEdges = Set(edge), routeParams = routeParams(defaultHeuristics, randomize = true), currentBlockHeight = blockHeight) match {
          case Success(_) => successes += 1
          case Failure(_) => failures += 1
        }
      }
      println(s"[mpp-balances fraction=$fraction] successes=$successes failures=$failures")
    }
  }

  test("smaller amounts, single-part, default heuristics") {
    val g = GraphWithBalanceEstimates(loadGraph(), 1 day)
    for (amountMsat <- Seq(158_239_000L, 79_119_000L)) {
      val recipient = randomKey().publicKey
      val edge = blindedEdge(recipient)
      val amount = MilliSatoshi(amountMsat)
      val maxFee = MilliSatoshi(4_000 + amountMsat * 4_000 / 1_000_000)
      val result = findRoute(g, acinq, recipient, amount, maxFee, numRoutes = DEFAULT_ROUTES_COUNT,
        extraEdges = Set(edge), routeParams = routeParams(defaultHeuristics, randomize = true), currentBlockHeight = blockHeight)
      printResult(s"single-part amount=$amountMsat maxFee=$maxFee", result, amount)
    }
  }
}
