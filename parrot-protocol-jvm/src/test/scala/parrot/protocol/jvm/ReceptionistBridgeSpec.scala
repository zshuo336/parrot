package parrot.protocol.jvm

import akka.actor.typed.receptionist.Receptionist
import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorRef, ActorSystem}
import org.scalatest.funsuite.AnyFunSuite
import scala.collection.mutable

/** ReceptionistBridge：akka listing diff → parrot Registered/Unregistered。 */
class ReceptionistBridgeSpec extends AnyFunSuite {

  test("key validation matches parrot rules") {
    assert(ReceptionistBridge.validateKey("jvm/echo"))
    assert(!ReceptionistBridge.validateKey("bad key"))
    assert(!ReceptionistBridge.validateKey("star*"))
    assert(!ReceptionistBridge.validateKey("hash#"))
    assert(!ReceptionistBridge.validateKey(""))
  }

  test("register and deregister produce Registered then Unregistered") {
    val sys = ActorSystem(Behaviors.empty[Any], "rec-bridge-test")
    try {
      val events = mutable.ListBuffer.empty[ReceptionistBridge.ParrotEvent]
      val bridge = sys.systemActorOf(
        ReceptionistBridge("jvm-gw-1", "jvm/echo", e => events.synchronized(events += e)),
        "bridge"
      )
      // echo actor：收到 "stop" 自终止（typed 行为内 ctx.stop）
      val echo = sys.systemActorOf(
        Behaviors.receiveMessage[Any] {
          case "stop" =>
            Behaviors.stopped(() => ())
          case _ => Behaviors.same
        },
        "rec-echo"
      )
      sys.receptionist ! Receptionist.Register(
        ReceptionistBridge.toServiceKey("jvm/echo"),
        echo
      )
      waitUntil(events.synchronized(
        events.exists {
          case ReceptionistBridge.Registered(k, p) => k == "jvm/echo" && p.contains("rec-echo")
          case _                                   => false
        }
      ))
      // 注销：echo 自 stop → receptionist 移除 → listing diff 产生 Unregistered
      echo ! "stop"
      waitUntil(events.synchronized(
        events.exists {
          case ReceptionistBridge.Unregistered(_, p) => p.contains("rec-echo")
          case _                                     => false
        }
      ))
    } finally sys.terminate()
  }

  private def waitUntil(cond: => Boolean, timeoutMs: Long = 5000): Unit = {
    val t0 = System.currentTimeMillis()
    while (!cond && System.currentTimeMillis() - t0 < timeoutMs) Thread.sleep(20)
    assert(cond, "condition not met within timeout")
  }
}
