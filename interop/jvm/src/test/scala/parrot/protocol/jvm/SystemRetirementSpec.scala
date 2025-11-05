package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.Behaviors
import io.netty.channel.embedded.EmbeddedChannel
import java.util.concurrent.{Callable, ConcurrentLinkedQueue, CountDownLatch, Executors, TimeUnit}
import java.util.concurrent.atomic.{AtomicBoolean, AtomicInteger}
import scala.concurrent.Await
import scala.concurrent.duration._
import org.scalatest.funsuite.AnyFunSuite

class SystemRetirementSpec extends AnyFunSuite {
  private def session(run: ActorSystem[Any] => Unit): Unit = {
    val system = ActorSystem[Any](Behaviors.empty, "retirement-contract")
    try run(system)
    finally { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
  }
  test("unregistration removes captures and cannot cancel another registration") {
    val registry = new SystemRetirement(error => throw error)
    val calls = new AtomicInteger
    val old = registry.register(() => fail("removed registration ran"))
    old.close(); old.close()
    val next = registry.register(() => { calls.incrementAndGet(); () })
    old.close()
    assert(registry.registeredCount == 1)
    registry.retire(); registry.retire(); next.close()
    assert(calls.get() == 1 && registry.registeredCount == 0)
    registry.register(() => { calls.incrementAndGet(); () }).close()
    assert(calls.get() == 2 && registry.registeredCount == 0)
  }
  test("cleanup failures do not prevent retirement of the other registered resources") {
    val errors = new ConcurrentLinkedQueue[Throwable]()
    val registry = new SystemRetirement(error => { errors.add(error); () })
    val failure = new IllegalStateException("cleanup failed")
    val calls = new AtomicInteger
    registry.register(() => throw failure)
    (1 to 10).foreach(_ => registry.register(() => { calls.incrementAndGet(); () }))
    registry.retire()
    assert(calls.get() == 10 && registry.registeredCount == 0)
    assert(errors.size() == 1 && (errors.poll() eq failure))
  }
  test("an observable terminal owner fences new registration before its Future callback runs") {
    val terminal = new AtomicBoolean(false)
    val registry = new SystemRetirement(error => throw error, () => terminal.get())
    val calls = new AtomicInteger
    registry.register(() => { calls.incrementAndGet(); () })
    terminal.set(true)
    // No retire notification yet: registration must reconcile terminal state.
    registry.register(() => { calls.incrementAndGet(); () }).close()
    assert(calls.get() == 2 && registry.registeredCount == 0)
    registry.retire(); assert(calls.get() == 2)
  }
  test("cleanup and already-retired registration execute outside the registry lock") {
    val errors = new ConcurrentLinkedQueue[Throwable]()
    val registry = new SystemRetirement(error => { errors.add(error); () })
    val pool = Executors.newSingleThreadExecutor()
    try {
      def cleanup(): Unit = {
        val otherThread = pool.submit(new Callable[Int] {
          def call(): Int = registry.registeredCount
        })
        assert(otherThread.get(3, TimeUnit.SECONDS) == 0)
      }
      registry.register(() => cleanup())
      registry.retire()
      registry.register(() => cleanup()).close()
      assert(errors.isEmpty, s"callback blocked registry progress: $errors")
    } finally { pool.shutdownNow(); assert(pool.awaitTermination(5, TimeUnit.SECONDS)) }
  }
  test("register versus retirement never loses or repeats an uncanceled cleanup") {
    val pool = Executors.newFixedThreadPool(2)
    try {
      for (_ <- 0 until 128) {
        val registry = new SystemRetirement(error => throw error)
        val calls = new AtomicInteger
        val start = new CountDownLatch(1)
        val add = pool.submit(new Callable[Unit] {
          def call(): Unit = { assert(start.await(5, TimeUnit.SECONDS)); registry.register(() => { calls.incrementAndGet(); () }); () }
        })
        val retire = pool.submit(new Callable[Unit] {
          def call(): Unit = { assert(start.await(5, TimeUnit.SECONDS)); registry.retire() }
        })
        start.countDown(); add.get(5, TimeUnit.SECONDS); retire.get(5, TimeUnit.SECONDS)
        registry.retire()
        assert(calls.get() == 1 && registry.registeredCount == 0)
      }
    } finally { pool.shutdownNow(); assert(pool.awaitTermination(5, TimeUnit.SECONDS)) }
  }
  test("unregister versus retirement allows at most one already-claimed callback") {
    val pool = Executors.newFixedThreadPool(2)
    try {
      for (_ <- 0 until 128) {
        val registry = new SystemRetirement(error => throw error)
        val calls = new AtomicInteger
        val token = registry.register(() => { calls.incrementAndGet(); () })
        val start = new CountDownLatch(1)
        val close = pool.submit(new Callable[Unit] {
          def call(): Unit = { assert(start.await(5, TimeUnit.SECONDS)); token.close() }
        })
        val retire = pool.submit(new Callable[Unit] {
          def call(): Unit = { assert(start.await(5, TimeUnit.SECONDS)); registry.retire() }
        })
        start.countDown(); close.get(5, TimeUnit.SECONDS); retire.get(5, TimeUnit.SECONDS)
        registry.retire(); token.close()
        assert(calls.get() >= 0 && calls.get() <= 1 && registry.registeredCount == 0)
      }
    } finally { pool.shutdownNow(); assert(pool.awaitTermination(5, TimeUnit.SECONDS)) }
  }
  test("closed budget remains registered only while its admitted tickets still need retirement") {
    session { system =>
      val registry = SystemRetirement(system)
      val budget = GatewayAskBudget.owned(system)
      val first = budget.connection().acquire(1, 10).toOption.get
      assert(registry.registeredCount == 1)
      budget.close(); assert(registry.registeredCount == 1)
      first.complete(fail("closed budget sent a reply"))
      assert(budget.snapshot == (0, 0L) && registry.registeredCount == 0)
      first.complete(fail("duplicate reply")); budget.close()
      assert(registry.registeredCount == 0)
    }
  }
  test("repeated resource retirement returns owner registration count to its baseline") {
    session { system =>
      val registry = SystemRetirement(system)
      val default = AdminV2Executor.ComponentRoutes(system)
      assert(registry.registeredCount == 1)
      for (_ <- 0 until 128) {
        val budget = GatewayAskBudget.owned(system)
        val routes = AdminV2Executor.ComponentRoutes.newScope(system)
        assert(registry.registeredCount == 3)
        budget.close(); routes.close()
        assert(registry.registeredCount == 1)
      }
      for (_ <- 0 until 12) {
        val transport = new ParrotTransportExtension(system, null, "registration-churn")
        transport.listen(0)
        assert(registry.registeredCount == 3)
        transport.shutdown().get(10, TimeUnit.SECONDS)
        assert(registry.registeredCount == 1)
      }
      default.close(); assert(registry.registeredCount == 0)
    }
  }
  for (client <- Seq(false, true); removed <- Seq(false, true)) {
    test(s"direct handler releases its private budget on retirement (client=$client, removed=$removed)") {
      session { system =>
        val bridge = system.systemActorOf(Behaviors.ignore[BridgeActor.BridgeMsg], "bridge")
        val registry = SystemRetirement(system)
        val handler = if (client) new ParrotClientHandler(bridge, "direct", None, system)
          else new ParrotServerHandler(bridge, "direct", None, system)
        assert(registry.registeredCount == 1)
        val channel = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), handler)
        try {
          if (removed) channel.pipeline().remove(handler) else channel.close()
          assert(registry.registeredCount == 0)
        } finally channel.finishAndReleaseAll()
      }
    }
  }
  test("removing one direct handler cannot close an explicitly shared budget") {
    session { system =>
      val bridge = system.systemActorOf(Behaviors.ignore[BridgeActor.BridgeMsg], "bridge")
      val registry = SystemRetirement(system)
      val budget = GatewayAskBudget.owned(system)
      val a = new EmbeddedChannel(new ParrotServerHandler(bridge, "a", None, system.scheduler, budget))
      val b = new EmbeddedChannel(new ParrotServerHandler(bridge, "b", None, system.scheduler, budget))
      try {
        a.finishAndReleaseAll()
        val probe = budget.connection().acquire(1, 10).toOption.get
        probe.complete(())
        assert(registry.registeredCount == 1)
        b.finishAndReleaseAll(); assert(registry.registeredCount == 1)
        budget.close(); assert(registry.registeredCount == 0)
      } finally { a.finishAndReleaseAll(); b.finishAndReleaseAll(); budget.close() }
    }
  }
  test("post-termination resources are immediately fenced without stale registrations") {
    session { system =>
      val registry = SystemRetirement(system)
      AdminV2Executor.ComponentRoutes(system)
      system.terminate(); Await.result(system.whenTerminated, 10.seconds)
      val budget = GatewayAskBudget.owned(system)
      val routes = AdminV2Executor.ComponentRoutes.newScope(system)
      assert(budget.connection().acquire(1, 0) == Left(GatewayAskBudget.Closed))
      assert(!routes.publish(Vector.empty))
      assert(registry.registeredCount == 0)
    }
  }
}
