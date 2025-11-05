package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.Behaviors
import java.lang.ref.{Reference, WeakReference}
import java.nio.file.{Files, Paths}
import java.util.concurrent.TimeUnit
import scala.concurrent.Await
import scala.concurrent.duration._
import org.scalatest.funsuite.AnyFunSuite

object OwnerRetentionProbe {
  private def retired(system: ActorSystem[Any], kind: String): Vector[WeakReference[AnyRef]] =
    (0 until 12).map { _ =>
      val value: AnyRef = kind match {
        case "transport" =>
          val transport = new ParrotTransportExtension(system, null, "retention")
          transport.listen(0)
          transport.shutdown().get(10, TimeUnit.SECONDS)
          transport
        case "budget" =>
          val budget = GatewayAskBudget.owned(system)
          budget.close()
          budget
        case "routes" =>
          val routes = AdminV2Executor.ComponentRoutes.newScope(system)
          routes.close()
          routes
      }
      new WeakReference(value)
    }.toVector

  private def controls(): Vector[WeakReference[AnyRef]] =
    (0 until 12).map(_ => new WeakReference[AnyRef](new Object)).toVector

  def main(args: Array[String]): Unit = {
    val owner = ActorSystem[Any](Behaviors.empty, "retained-owner")
    try {
      val refs = retired(owner, args(0))
      val control = controls()
      // Separate methods release construction locals. The subprocess explicitly
      // selects SerialGC and permits full GC; controls prove collection ran while
      // the still-live ActorSystem must remain available for further work.
      for (_ <- 0 until 40 if (refs ++ control).exists(_.get() != null)) {
        System.gc()
        Thread.sleep(25)
      }
      assert(control.forall(_.get() == null), "GC control objects did not retire")
      assert(refs.forall(_.get() == null), s"${args(0)} retained after close: ${refs.count(_.get() != null)}/${refs.size}")
      assert(!owner.whenTerminated.isCompleted)
      Reference.reachabilityFence(owner)
    } finally { owner.terminate(); Await.result(owner.whenTerminated, 10.seconds) }
  }
}

class OwnerRetentionSpec extends AnyFunSuite {
  for (kind <- Seq("transport", "budget", "routes")) {
    test(s"closed $kind objects are collectible while their owning ActorSystem remains alive") {
      val log = Files.createTempFile("parrot-owner-retention-", ".log")
      val java = Paths.get(System.getProperty("java.home"), "bin", "java").toString
      val child = new ProcessBuilder(java, "-Xmx96m", "-XX:+UseSerialGC", "-XX:-DisableExplicitGC",
        "-cp", System.getProperty("java.class.path"), "parrot.protocol.jvm.OwnerRetentionProbe", kind)
        .redirectErrorStream(true).redirectOutput(log.toFile).start()
      try {
        assert(child.waitFor(30, TimeUnit.SECONDS), s"retention subprocess timeout: $log")
        assert(child.exitValue() == 0, s"retention subprocess failed: ${Files.readString(log)}")
      } finally {
        if (child.isAlive) { child.destroyForcibly(); assert(child.waitFor(5, TimeUnit.SECONDS)) }
      }
    }
  }
}
