package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import java.util.concurrent.{Callable, CountDownLatch, Executors, TimeUnit}

class GatewayAskBudgetSpec extends AnyFunSuite {
  test("invalid capacities are rejected instead of disabling bounds") {
    intercept[IllegalArgumentException](GatewayAskLimits(requests = 0))
    intercept[IllegalArgumentException](GatewayAskLimits(bytes = 0))
    intercept[IllegalArgumentException](GatewayAskLimits(connectionRequests = 0))
    intercept[IllegalArgumentException](GatewayAskLimits(connectionBytes = 0))
    intercept[IllegalArgumentException](GatewayAskLimits(requests = 1, connectionRequests = 2))
    intercept[IllegalArgumentException](GatewayAskLimits(bytes = 1, connectionBytes = 2))
  }
  test("receipt exceptions and repeated completion release a ticket exactly once") {
    val budget = new GatewayAskBudget(GatewayAskLimits(1, 10, 1, 10))
    val connection = budget.connection()
    val ticket = connection.acquire(1, 10).toOption.get
    assert(connection.acquire(1, 0) == Left(GatewayAskBudget.Duplicate))
    intercept[IllegalStateException](ticket.complete(throw new IllegalStateException("write failed")))
    assert(budget.snapshot == (0, 0L))
    ticket.complete(fail("terminal callback repeated"))
    val next = connection.acquire(1, 10).toOption.get
    ticket.complete(fail("old completion removed replacement"))
    assert(budget.snapshot == (1, 10L))
    next.complete(()); assert(budget.snapshot == (0, 0L))
  }
  test("connection and transport close suppress replies but retain live waiters in the budget") {
    val budget = new GatewayAskBudget(GatewayAskLimits(2, 20, 1, 10))
    val a = budget.connection(); val b = budget.connection()
    val first = a.acquire(1, 10).toOption.get
    a.close()
    assert(a.acquire(2, 0) == Left(GatewayAskBudget.Closed))
    assert(budget.snapshot == (1, 10L))
    val second = b.acquire(1, 10).toOption.get
    first.complete(fail("disconnected request sent a response"))
    assert(budget.snapshot == (1, 10L))
    budget.close()
    assert(budget.connection().acquire(3, 0) == Left(GatewayAskBudget.Closed))
    second.complete(fail("retired transport sent a response"))
    assert(budget.snapshot == (0, 0L))
  }
  test("concurrent connections cannot overbook the global byte budget") {
    val budget = new GatewayAskBudget(GatewayAskLimits(10, 10, 1, 10))
    val pool = Executors.newFixedThreadPool(16)
    val start = new CountDownLatch(1)
    try {
      val attempts = (0 until 16).map { _ =>
        pool.submit(new Callable[Option[() => Unit]] {
          def call(): Option[() => Unit] = {
            assert(start.await(5, TimeUnit.SECONDS))
            budget.connection().acquire(1, 3).toOption.map(ticket => () => ticket.complete(()))
          }
        })
      }
      start.countDown()
      val admitted = attempts.flatMap(_.get(5, TimeUnit.SECONDS))
      assert(admitted.size == 3 && budget.snapshot == (3, 9L))
      admitted.foreach(_())
      assert(budget.snapshot == (0, 0L))
    } finally { start.countDown(); pool.shutdownNow(); assert(pool.awaitTermination(5, TimeUnit.SECONDS)) }
  }
  test("owner retirement fences callbacks and cannot double-release a concurrently completing receipt") {
    val budget = new GatewayAskBudget(GatewayAskLimits(2, 20, 2, 20))
    val connection = budget.connection()
    val first = connection.acquire(1, 10).toOption.get
    val second = connection.acquire(2, 10).toOption.get
    first.complete {
      // Receipt construction is outside the ledger lock; owner retirement may
      // happen here, before complete's finally block returns the original ticket.
      budget.ownerTerminated()
      assert(budget.snapshot == (0, 0L))
    }
    second.complete(fail("retired callback ran"))
    first.complete(fail("completed callback ran again"))
    assert(budget.snapshot == (0, 0L))
    assert(connection.acquire(1, 0) == Left(GatewayAskBudget.Closed))
  }
  test("separate transport budgets do not share capacity") {
    val first = new GatewayAskBudget(GatewayAskLimits(1, 1, 1, 1))
    val second = new GatewayAskBudget(GatewayAskLimits(1, 1, 1, 1))
    val a = first.connection().acquire(1, 1).toOption.get
    val b = second.connection().acquire(1, 1).toOption.get
    first.close(); a.complete(())
    assert(second.snapshot == (1, 1L))
    b.complete(()); assert(second.snapshot == (0, 0L))
  }
}
