package parrot.protocol.jvm

import akka.actor.typed.{ActorRef, ActorSystem}
import akka.actor.typed.scaladsl.Behaviors
import org.scalatest.funsuite.AnyFunSuite
import java.util.concurrent.{LinkedBlockingQueue, TimeUnit}
import java.util.concurrent.atomic.AtomicBoolean
import scala.concurrent.{Await, ExecutionContext, Future, Promise}
import scala.concurrent.duration._
import AdminV2Executor.ComponentRoutes

class ComponentRouteScopeSpec extends AnyFunSuite {
  private def session(run: ActorSystem[Any] => Unit): Unit = {
    val system = ActorSystem[Any](Behaviors.empty, "route-contract")
    try run(system)
    finally { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
  }
  private def tree(system: ActorSystem[Any], name: String): (ActorRef[Any], ActorRef[Any]) = {
    val ready = Promise[ActorRef[Any]]()
    val root = system.systemActorOf(Behaviors.setup[Any] { outer =>
      outer.spawn(Behaviors.setup[Any] { impl =>
        ready.success(impl.spawn(Behaviors.empty[Any], "child")); Behaviors.empty[Any]
      }, "impl")
      Behaviors.empty[Any]
    }, name)
    (root, Await.result(ready.future, 5.seconds))
  }
  private class HeldExecutor extends ExecutionContext {
    private val tasks = new LinkedBlockingQueue[Runnable]()
    def execute(task: Runnable): Unit = { tasks.add(task); () }
    def reportFailure(error: Throwable): Unit = throw new AssertionError("lookup callback failed", error)
    def next(): Runnable = {
      val task = tasks.poll(5, TimeUnit.SECONDS)
      assert(task != null, "lookup never reached its completion boundary")
      task
    }
    def finish[A](future: Future[A]): A = {
      while (!future.isCompleted) next().run()
      Await.result(future, 1.second)
    }
  }

  test("idempotent put cannot overwrite ownership and stale retirement cannot remove a replacement") {
    session { system =>
      val routes = ComponentRoutes(system)
      val (old, _) = tree(system, "old"); val (next, _) = tree(system, "next")
      assert(routes.put("logical", old)); assert(routes.put("logical", old))
      assert(!routes.put("logical", next)); assert(routes.get("logical").contains(old))
      routes.removeOwned("logical", next); assert(routes.get("logical").contains(old))
      routes.removeOwned("logical", old); assert(routes.put("logical", next))
      val alive = new AtomicBoolean(true)
      routes.retired("logical", old, alive)
      assert(!alive.get()); assert(routes.get("logical").contains(next))
    }
  }
  test("publication is all-or-nothing for duplicates dead instances and owned paths") {
    session { system =>
      val routes = ComponentRoutes(system)
      val (root, _) = tree(system, "root")
      def entry(path: String, alive: Boolean = true) = (path, root, new AtomicBoolean(alive))
      assert(routes.publish(Vector(entry("owned"))))
      for (entries <- Seq(Vector(entry("free"), entry("owned")),
                           Vector(entry("free"), entry("free")),
                           Vector(entry("free"), entry("dead", false)))) {
        assert(!routes.publish(entries)); assert(routes.get("free").isEmpty)
        assert(routes.get("dead").isEmpty); assert(routes.get("owned").contains(root))
      }
    }
  }
  test("closed scopes reject both registration paths without closing another scope") {
    session { system =>
      val routes = ComponentRoutes.newScope(system)
      val other = ComponentRoutes(system)
      val (root, _) = tree(system, "root")
      assert(routes.put("logical", root)); assert(other.put("logical", root))
      routes.close(); routes.close()
      assert(routes.get("logical").isEmpty)
      assert(!routes.put("logical", root))
      assert(!routes.publish(Vector(("new", root, new AtomicBoolean(true)))))
      assert(other.get("logical").contains(root))
    }
  }
  test("default and explicit scopes are fenced after their owning system terminates") {
    val system = ActorSystem[Any](Behaviors.empty, "terminated-scope")
    val routes = Seq(ComponentRoutes(system), ComponentRoutes.newScope(system))
    val (root, _) = tree(system, "root")
    routes.foreach(r => assert(r.put("logical", root)))
    system.terminate(); Await.result(system.whenTerminated, 10.seconds)
    // whenTerminated callbacks need not precede other waiters; use the actor
    // system dispatcher-independent completion observation before checking all scopes.
    val deadline = System.nanoTime() + 5.seconds.toNanos
    while (routes.exists(_.get("logical").nonEmpty) && System.nanoTime() < deadline) java.util.concurrent.locks.LockSupport.parkNanos(1000000L)
    routes.foreach { r =>
      assert(r.get("logical").isEmpty)
      assert(!r.put("logical", root))
      assert(!r.publish(Vector(("new", root, new AtomicBoolean(true)))))
    }
  }
  test("AdminPort rejects a foreign ActorSystem route scope before spawning components") {
    session { a => session { b =>
      intercept[IllegalArgumentException](AdminPort(b, ComponentRoutes(a)))
      intercept[IllegalArgumentException](AdminPort(b, ComponentRoutes.newScope(a)))
      assert(AdminPort(b, ComponentRoutes(b)) != null)
    }}
  }
  for (change <- Seq("unchanged", "remove", "replace", "close", "remove-and-reinsert-same-ref")) {
    test(s"async child resolution rechecks publication after $change") {
      session { system =>
        val routes = ComponentRoutes(system)
        val (root, child) = tree(system, "original")
        val (replacement, _) = tree(system, "replacement")
        assert(routes.put("logical", root))
        val executor = new HeldExecutor
        val lookup = routes.resolveChild("logical/child")(executor)
        val completion = executor.next()
        change match {
          case "unchanged" => ()
          case "remove" => routes.removeOwned("logical", root)
          case "replace" => routes.removeOwned("logical", root); assert(routes.put("logical", replacement))
          case "close" => routes.close()
          case _ => routes.removeOwned("logical", root); assert(routes.put("logical", root))
        }
        completion.run()
        assert(executor.finish(lookup) == (if (change == "unchanged") Some(child) else None))
      }
    }
  }
}
