package parrot.protocol.jvm

import akka.actor.typed.{ActorSystem, ActorRef, Behavior, PostStop}
import akka.actor.typed.scaladsl.{Behaviors, AskPattern}
import akka.actor.typed.scaladsl.adapter._
import AskPattern._
import org.scalatest.funsuite.AnyFunSuite
import java.util.concurrent.{CountDownLatch, LinkedBlockingQueue, TimeUnit}
import java.util.concurrent.atomic.{AtomicInteger, AtomicReference}
import scala.concurrent.Await
import scala.concurrent.duration._
import AdminV2Codec._
import AdminV2Codec.AdminCommandV2._
import AdminV2Codec.AdminReplyV2._
import AdminV2Executor._
import BridgeActor._

class DeploymentContractsSpec extends AnyFunSuite {
  import DeploymentContractsSpec._
  def session(run: (ActorSystem[Any], ActorRef[AdminPort.Msg]) => Unit): Unit = withLimits(AdminPort.Limits())(run)
  def withLimits(limits: AdminPort.Limits, singleThread: Boolean = false)(run: (ActorSystem[Any], ActorRef[AdminPort.Msg]) => Unit): Unit = {
    val config = com.typesafe.config.ConfigFactory.parseString(if (singleThread)
      "akka.actor.default-dispatcher.executor = thread-pool-executor\nakka.actor.default-dispatcher.thread-pool-executor.fixed-pool-size = 1" else "")
    val system = ActorSystem(Behaviors.empty[Any], "deployment-contract", config)
    val port = system.systemActorOf(AdminPort(system, limits), "admin")
    try run(system, port)
    finally { release.countDown(); system.terminate(); Await.result(system.whenTerminated, 12.seconds) }
  }
  def pending(port: ActorRef[AdminPort.Msg], cmd: AdminCommandV2): LinkedBlockingQueue[AdminReplyV2] = {
    val replies = new LinkedBlockingQueue[AdminReplyV2]
    port ! AdminPort.CmdIn(cmd.reqId, "/test/admin", encodeCommand(cmd), (_, _, bytes) => { replies.add(decodeReply(bytes)); () })
    replies
  }
  def ask(port: ActorRef[AdminPort.Msg], cmd: AdminCommandV2): AdminReplyV2 = {
    val reply = pending(port, cmd).poll(10, TimeUnit.SECONDS)
    assert(reply != null, "no terminal admin reply")
    reply
  }
  def deploy(name: String, cls: Class[_], count: Option[Long] = None, config: String = "value") =
    Deploy(1, ComponentDeploy(name, "v", ArtifactRef.Jvm(cls.getName, None),
      count.map(InstancePolicy.Pool).getOrElse(InstancePolicy.Singleton), Some(config.getBytes("UTF-8"))))
  def expectFailure(reply: AdminReplyV2, code: Int = ErrSpawnFailed): Unit = reply match {
    case Failed(_, c, _) => assert(c == code)
    case other => assert(false, s"expected Failed($code), got $other")
  }
  def eventually(predicate: => Boolean): Unit = {
    val end = System.nanoTime() + 5.seconds.toNanos
    while (!predicate && System.nanoTime() < end) Thread.sleep(5)
    assert(predicate)
  }
  def echo(system: ActorSystem[Any], path: String): String = {
    val ref = ComponentRoutes(system).get(path).getOrElse(throw new AssertionError("route missing: " + path))
    implicit val timeout: akka.util.Timeout = 1.second
    implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
    Await.result(ref.ask[Any](r => BridgeAsk("test:echo", Array.emptyByteArray, r)), 2.seconds) match {
      case BridgeReplyOk(_, bytes) => new String(bytes, "UTF-8")
      case other => throw new AssertionError(s"bad business reply $other")
    }
  }
  for ((name, cls) <- Vector("behavior" -> classOf[ThrowBehavior], "setup" -> classOf[ThrowSetup], "stopped" -> classOf[StoppedInitially])) {
    test(s"$name initialization failure is not Deployed") {
      session { (system, port) =>
        val reply = ask(port, deploy("failure-" + name, cls))
        expectFailure(reply)
        if (name != "stopped") assert(reply.asInstanceOf[Failed].detail.contains(name + " failed"), s"lost initializer diagnostic: $reply")
        assert(ComponentRoutes(system).get("failure-" + name).isEmpty)
        expectFailure(ask(port, Status(2, "/user/failure-" + name)), ErrComponentNotFound)
      }
    }
  }
  test("partial pool constructor failure retires every instance and route") {
    constructors.set(0); retired = new CountDownLatch(1)
    session { (system, port) =>
      expectFailure(ask(port, deploy("pool-rollback", classOf[ThrowSecondConstructor], Some(3))))
      assert(retired.await(5, TimeUnit.SECONDS), "prefix instance never retired")
      assert((0 until 3).forall(i => ComponentRoutes(system).get(s"pool-rollback-$i").isEmpty))
      expectFailure(ask(port, Status(2, "/user/pool-rollback")), ErrComponentNotFound)
    }
  }
  test("Deployed and all routes wait for the entire pool readiness barrier") {
    entered = new CountDownLatch(1); release = new CountDownLatch(1)
    session { (system, port) =>
      val replies = pending(port, deploy("pool-gate", classOf[GateSecond], Some(2)))
      try {
        assert(entered.await(5, TimeUnit.SECONDS), "second instance never started")
        assert(replies.poll(150, TimeUnit.MILLISECONDS) == null, "Deployed preceded setup completion")
        assert(ComponentRoutes(system).get("pool-gate-0").isEmpty && ComponentRoutes(system).get("pool-gate-1").isEmpty,
          "uncommitted pool was partially routable")
      } finally release.countDown()
      assert(replies.poll(5, TimeUnit.SECONDS).isInstanceOf[Deployed])
      assert(echo(system, "pool-gate-0") == "value" && echo(system, "pool-gate-1") == "value")
    }
  }
  test("invalid replacement preflight preserves the running component") {
    session { (system, port) =>
      assert(ask(port, deploy("preflight", classOf[Echo])).isInstanceOf[Deployed])
      for (invalid <- Vector(classOf[String], classOf[AbstractEcho], classOf[PrivateEcho]))
        expectFailure(ask(port, deploy("preflight", invalid)))
      assert(echo(system, "preflight") == "value")
    }
  }
  test("failed replacement setup does not retain stale running registration") {
    session { (system, port) =>
      assert(ask(port, deploy("replacement", classOf[Echo])).isInstanceOf[Deployed])
      expectFailure(ask(port, deploy("replacement", classOf[ThrowSetup])))
      expectFailure(ask(port, Status(2, "/user/replacement")), ErrComponentNotFound)
      assert(ComponentRoutes(system).get("replacement").isEmpty)
    }
  }
  test("Stop one pool path preserves other live instances") {
    session { (system, port) =>
      assert(ask(port, deploy("subset", classOf[Echo], Some(2))).isInstanceOf[Deployed])
      assert(ask(port, Stop(2, "/user/subset-0")).isInstanceOf[Stopped])
      assert(ComponentRoutes(system).get("subset-0").isEmpty)
      assert(echo(system, "subset-1") == "value")
      ask(port, Status(3, "/user/subset")) match {
        case StatusReply(_, states) => assert(states.map(_.path) == Vector("/user/subset-1"))
        case other => assert(false, s"survivor missing: $other")
      }
    }
  }
  test("another component cannot overwrite an existing logical pool path") {
    session { (system, port) =>
      assert(ask(port, deploy("collision", classOf[Echo], Some(2))).isInstanceOf[Deployed])
      expectFailure(ask(port, deploy("collision-0", classOf[Echo], config = "intruder")))
      assert(echo(system, "collision-0") == "value")
    }
  }
  test("empty and overflowing instance counts never deploy empty success") {
    session { (system, port) =>
      for (n <- Vector(0L, 1L << 32, -1L)) expectFailure(ask(port, deploy("bad-count", classOf[Echo], Some(n))))
    }
  }
  test("terminated implementation retires wrapper route and registry") {
    retired = new CountDownLatch(1)
    session { (system, port) =>
      assert(ask(port, deploy("self-stop", classOf[StopOnTell])).isInstanceOf[Deployed])
      ComponentRoutes(system).get("self-stop").get ! BridgeTell("test:stop", Array.emptyByteArray)
      assert(retired.await(5, TimeUnit.SECONDS))
      eventually(ComponentRoutes(system).get("self-stop").isEmpty)
      eventually(ask(port, Status(2, "/user/self-stop")).isInstanceOf[Failed])
    }
  }
  test("startup timeout preserves a retiring handle until blocked setup actually exits") {
    entered = new CountDownLatch(1); release = new CountDownLatch(1)
    withLimits(AdminPort.Limits(startup = 150.millis, retirement = 150.millis)) { (system, port) =>
      val reply = pending(port, deploy("timeout", classOf[GateSetup]))
      assert(entered.await(5, TimeUnit.SECONDS))
      expectFailure(reply.poll(3, TimeUnit.SECONDS))
      assert(ComponentRoutes(system).get("timeout").isEmpty)
      ask(port, Status(2, "/user/timeout")) match {
        case StatusReply(_, states) => assert(states.map(_.state) == Vector("retiring"))
        case other => fail(s"lost live retiring handle: $other")
      }
      expectFailure(ask(port, deploy("timeout", classOf[Echo])), ErrDrainTimeout)
      release.countDown()
      eventually(ask(port, Status(3, "/user/timeout")).isInstanceOf[Failed])
      assert(ask(port, deploy("timeout", classOf[Echo], config = "recovered")).isInstanceOf[Deployed])
      assert(echo(system, "timeout") == "recovered")
    }
  }
  test("pool retirement before publication aborts the whole cohort") {
    firstImpl.set(null); entered = new CountDownLatch(1); release = new CountDownLatch(1)
    session { (system, port) =>
      val replies = pending(port, deploy("dies-before-publish", classOf[StopFirstGateSecond], Some(2)))
      assert(entered.await(5, TimeUnit.SECONDS))
      val first = firstImpl.get()
      assert(first != null)
      first ! "stop"
      eventually(scala.util.Try(Await.result(system.toClassic.actorSelection(first.path.parent)
        .resolveOne(100.millis), 200.millis)).isFailure)
      release.countDown()
      expectFailure(replies.poll(5, TimeUnit.SECONDS))
      assert(ComponentRoutes(system).get("dies-before-publish-0").isEmpty)
      assert(ComponentRoutes(system).get("dies-before-publish-1").isEmpty)
      expectFailure(ask(port, Status(2, "/user/dies-before-publish")), ErrComponentNotFound)
    }
  }
  test("Stop timeout retains actual retiring instance and preserves its pool sibling") {
    entered = new CountDownLatch(1); release = new CountDownLatch(1)
    withLimits(AdminPort.Limits(retirement = 150.millis)) { (system, port) =>
      assert(ask(port, deploy("slow-stop", classOf[GatePostStop], Some(2))).isInstanceOf[Deployed])
      val replies = pending(port, Stop(2, "/user/slow-stop-0"))
      assert(entered.await(5, TimeUnit.SECONDS))
      expectFailure(replies.poll(3, TimeUnit.SECONDS), ErrDrainTimeout)
      assert(ComponentRoutes(system).get("slow-stop-0").isEmpty)
      assert(echo(system, "slow-stop-1") == "value")
      ask(port, Status(3, "/user/slow-stop")) match {
        case StatusReply(_, states) => assert(states.map(s => s.path -> s.state).toMap ==
          Map("/user/slow-stop-0" -> "retiring", "/user/slow-stop-1" -> "running"))
        case other => fail(s"inaccurate retirement status: $other")
      }
      release.countDown()
      eventually(ask(port, Status(4, "/user/slow-stop-0")).isInstanceOf[Failed])
      assert(echo(system, "slow-stop-1") == "value")
    }
  }
  test("synchronously blocked Drain hook respects deadline and does not drain a sibling") {
    entered = new CountDownLatch(1); release = new CountDownLatch(1)
    session { (system, port) =>
      assert(ask(port, deploy("slow-drain", classOf[GateDrain], Some(2))).isInstanceOf[Deployed])
      val replies = pending(port, Drain(2, "/user/slow-drain-0", 150))
      assert(entered.await(5, TimeUnit.SECONDS))
      assert(replies.poll(3, TimeUnit.SECONDS) == Drained(2, 0, 1))
      assert(ComponentRoutes(system).get("slow-drain-0").isEmpty)
      assert(echo(system, "slow-drain-1") == "value")
      ask(port, Status(3, "/user/slow-drain-0")) match {
        case StatusReply(_, states) => assert(states.map(_.state) == Vector("draining"))
        case other => fail(s"inaccurate drain status: $other")
      }
      release.countDown()
      assert(ask(port, Stop(4, "/user/slow-drain-0")).isInstanceOf[Stopped])
      assert(echo(system, "slow-drain-1") == "value")
    }
  }
  test("internal command count budget rejects overflow while deployment is pending") {
    entered = new CountDownLatch(1); release = new CountDownLatch(1)
    withLimits(AdminPort.Limits(pendingCommands = 2)) { (_, port) =>
      val deployed = pending(port, deploy("queue-gate", classOf[GateSetup]))
      assert(entered.await(5, TimeUnit.SECONDS))
      val queued = pending(port, Status(2, "/user/queue-gate"))
      expectFailure(ask(port, Status(3, "/user/queue-gate")))
      assert(queued.poll() == null && deployed.poll() == null)
      release.countDown()
      assert(deployed.poll(5, TimeUnit.SECONDS).isInstanceOf[Deployed])
      assert(queued.poll(5, TimeUnit.SECONDS).isInstanceOf[StatusReply])
      assert(ask(port, Stop(4, "/user/queue-gate")).isInstanceOf[Stopped])
    }
  }
  test("internal command byte and instance budgets reject before component side effects") {
    constructors.set(0)
    withLimits(AdminPort.Limits(pendingBytes = 512, instances = 1)) { (_, port) =>
      expectFailure(ask(port, deploy("too-large", classOf[ThrowSecondConstructor], config = "x" * 1024)))
      expectFailure(ask(port, deploy("too-many", classOf[ThrowSecondConstructor], Some(2))))
      expectFailure(ask(port, deploy("pool-limit", classOf[ThrowSecondConstructor], Some(257))))
      assert(constructors.get() == 0)
      assert(ask(port, deploy("fits", classOf[Echo])).isInstanceOf[Deployed])
    }
  }
  test("management waiting does not deadlock a single-thread default dispatcher") {
    withLimits(AdminPort.Limits(startup = 2.seconds), singleThread = true) { (system, port) =>
      assert(ask(port, deploy("one-thread", classOf[Echo])).isInstanceOf[Deployed])
      assert(echo(system, "one-thread") == "value")
      assert(ask(port, Stop(2, "/user/one-thread")).isInstanceOf[Stopped])
    }
  }
  test("mismatched admin IDs are rejected before operations and reply with the frame ID") {
    session { (system, port) =>
      assert(ask(port, deploy("correlation-kept", classOf[Echo])).isInstanceOf[Deployed])
      val commands = Vector[AdminCommandV2](deploy("correlation-forged", classOf[Echo]),
        Stop(1, "/user/correlation-kept"), Drain(1, "/user/correlation-kept", 1000),
        Status(1, "/user/correlation-kept"), MetricsReport(1), MeasuredMetricsReport(1))
      commands.zipWithIndex.foreach { case (command, index) =>
        val cid = 100L + index
        val replies = new LinkedBlockingQueue[(Long, AdminReplyV2)]
        port ! AdminPort.CmdIn(cid, "/test/admin", encodeCommand(command), (outer, _, bytes) => {
          replies.add((outer, decodeReply(bytes))); ()
        })
        val received = replies.poll(5, TimeUnit.SECONDS)
        assert(received != null && received._1 == cid)
        received._2 match {
          case Failed(id, code, _) => assert(id == cid && code == ErrDialectMismatch)
          case other => fail(s"mismatched command executed: $other")
        }
        assert(echo(system, "correlation-kept") == "value")
        assert(ComponentRoutes(system).get("correlation-forged").isEmpty)
      }
      assert(ask(port, Stop(200, "/user/correlation-kept")).isInstanceOf[Stopped])
    }
  }

  test("lost reply sink does not roll back a committed deployment") {
    session { (system, port) =>
      val command = deploy("lost-sink", classOf[Echo])
      val replied = new CountDownLatch(1)
      port ! AdminPort.CmdIn(1, "/test/admin", encodeCommand(command), (_, _, _) => {
        replied.countDown(); throw new IllegalStateException("connection closed")
      })
      assert(replied.await(5, TimeUnit.SECONDS))
      assert(ask(port, Status(2, "/user/lost-sink")).isInstanceOf[StatusReply])
      assert(echo(system, "lost-sink") == "value")
    }
  }
  test("stopping the admin owner retires its component subtree") {
    retired = new CountDownLatch(1)
    session { (system, port) =>
      assert(ask(port, deploy("owned", classOf[TrackedEcho])).isInstanceOf[Deployed])
      system.toClassic.stop(port.toClassic)
      assert(retired.await(5, TimeUnit.SECONDS), "component escaped its management owner")
      eventually(ComponentRoutes(system).get("owned").isEmpty)
    }
  }
}

object DeploymentContractsSpec {
  val constructors = new AtomicInteger
  val firstImpl = new AtomicReference[ActorRef[Any]]
  @volatile var retired = new CountDownLatch(1)
  @volatile var entered = new CountDownLatch(1)
  @volatile var release = new CountDownLatch(0)
  class Echo extends ComponentSpi {
    override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.receiveMessage {
      case BridgeAsk(_, _, replyTo) => replyTo ! BridgeReplyOk("test:echo", ctx.config.get); Behaviors.same
      case _ => Behaviors.same
    }
  }
  abstract class AbstractEcho extends Echo
  class PrivateEcho private() extends Echo
  class ThrowBehavior extends ComponentSpi {
    override def behavior(ctx: ComponentContext): Behavior[Any] = throw new IllegalStateException("behavior failed")
  }
  class ThrowSetup extends ComponentSpi {
    override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.setup { _ => throw new IllegalStateException("setup failed") }
  }
  class StoppedInitially extends ComponentSpi {
    override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.stopped
  }
  class TrackedEcho extends Echo {
    override def behavior(ctx: ComponentContext): Behavior[Any] =
      Behaviors.receiveMessage[Any] {
        case BridgeAsk(_, _, replyTo) => replyTo ! BridgeReplyOk("test:echo", ctx.config.get); Behaviors.same
        case _ => Behaviors.same
      }.receiveSignal { case (_, PostStop) => retired.countDown(); Behaviors.same }
  }
  class ThrowSecondConstructor extends TrackedEcho {
    if (constructors.incrementAndGet() == 2) throw new IllegalStateException("second constructor failed")
  }
  class GateSecond extends Echo {
    override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.setup { _ =>
      if (ctx.path.endsWith("-1")) { entered.countDown(); if (!release.await(5, TimeUnit.SECONDS)) throw new IllegalStateException("gate deadline") }
      super.behavior(ctx)
    }
  }
  class GateSetup extends Echo {
    override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.setup { _ =>
      entered.countDown()
      if (!release.await(10, TimeUnit.SECONDS)) throw new IllegalStateException("setup gate deadline")
      super.behavior(ctx)
    }
  }
  class StopFirstGateSecond extends Echo {
    override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.setup { context =>
      if (ctx.path.endsWith("-0")) firstImpl.set(context.self)
      else { entered.countDown(); if (!release.await(10, TimeUnit.SECONDS)) throw new IllegalStateException("pool gate deadline") }
      Behaviors.receiveMessage { case "stop" => Behaviors.stopped; case _ => Behaviors.same }
    }
  }
  class GatePostStop extends Echo {
    override def behavior(ctx: ComponentContext): Behavior[Any] =
      Behaviors.receiveMessage[Any] {
        case BridgeAsk(_, _, replyTo) => replyTo ! BridgeReplyOk("test:echo", ctx.config.get); Behaviors.same
        case _ => Behaviors.same
      }.receiveSignal { case (_, PostStop) =>
        if (ctx.path.endsWith("-0")) { entered.countDown(); release.await(10, TimeUnit.SECONDS) }
        Behaviors.same
      }
  }
  class GateDrain extends Echo {
    override def parrotDrain(): java.util.concurrent.CompletableFuture[_] = {
      entered.countDown(); release.await(10, TimeUnit.SECONDS)
      java.util.concurrent.CompletableFuture.completedFuture(null)
    }
  }
  class StopOnTell extends ComponentSpi {
    override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.receiveMessage[Any] {
      case _: BridgeTell => Behaviors.stopped
      case _ => Behaviors.same
    }.receiveSignal { case (_, PostStop) => retired.countDown(); Behaviors.same }
  }
}
