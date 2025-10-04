package parrot.protocol.jvm

import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorRef, ActorSystem, Behavior}
import BridgeActor._
import org.scalatest.funsuite.AnyFunSuite
import scala.concurrent.Await
import scala.concurrent.duration._

/** 桥语义集成：echo（ASK→REPLY）/ cpu（载荷计算）/ tell / stop-unsupported。 */
class BridgeEchoSpec extends AnyFunSuite {

  def echoAny: Behavior[Any] = Behaviors.receiveMessage {
    case BridgeAsk(_, payload, replyTo) =>
      replyTo ! BridgeReplyOk("pb:parrot.protocol.v1.AkkaEnvelope", payload)
      Behaviors.same
    case BridgeTell(_, _) => Behaviors.same
    case _                => Behaviors.same
  }

  def cpuAny: Behavior[Any] = Behaviors.receiveMessage {
    case BridgeAsk(_, payload, replyTo) =>
      val n = if (payload.length == 8) {
        var v = 0L
        var i = 0
        while (i < 8) { v = (v << 8) | (payload(i).toLong & 0xFF); i += 1 }
        v
      } else 0L
      replyTo ! BridgeReplyOk(
        "pb:parrot.protocol.v1.AkkaEnvelope",
        java.nio.ByteBuffer.allocate(8).putLong(n + 1).array()
      )
      Behaviors.same
    case BridgeTell(_, _) => Behaviors.same
    case _                => Behaviors.same
  }

  test("ask echo roundtrip via bridge") {
    val sys = ActorSystem(Behaviors.empty[Any], "bridge-echo-test")
    try {
      val echo = sys.systemActorOf(echoAny, "echo")
      val bridge = sys.systemActorOf(BridgeActor(_ => Some(echo)), "bridge")
      implicit val timeout: akka.util.Timeout = 3.seconds
      implicit val sch: akka.actor.typed.Scheduler = sys.scheduler
      import akka.actor.typed.scaladsl.AskPattern._
      val fut = bridge.ask[BridgeReply](ref => Ask(7L, "echo", "pb:x", "hi".getBytes, ref))
      Await.result(fut, 3.seconds) match {
        case Replied(cid, _, payload) =>
          assert(cid == 7L)
          assert(new String(payload) == "hi")
        case other => fail(s"unexpected: $other")
      }
    } finally sys.terminate()
  }

  test("ask cpu computes") {
    val sys = ActorSystem(Behaviors.empty[Any], "bridge-cpu-test")
    try {
      val cpu = sys.systemActorOf(cpuAny, "cpu")
      val bridge = sys.systemActorOf(BridgeActor(_ => Some(cpu)), "bridge")
      implicit val timeout: akka.util.Timeout = 3.seconds
      implicit val sch: akka.actor.typed.Scheduler = sys.scheduler
      import akka.actor.typed.scaladsl.AskPattern._
      val in = java.nio.ByteBuffer.allocate(8).putLong(41L).array()
      val fut = bridge.ask[BridgeReply](ref => Ask(1L, "cpu", "pb:x", in, ref))
      Await.result(fut, 3.seconds) match {
        case Replied(_, _, payload) =>
          val out = java.nio.ByteBuffer.wrap(payload).getLong()
          assert(out == 42L)
        case other => fail(s"unexpected: $other")
      }
    } finally sys.terminate()
  }

  test("ask unknown actor → ActorNotFound") {
    val sys = ActorSystem(Behaviors.empty[Any], "bridge-ghost-test")
    try {
      val bridge = sys.systemActorOf(BridgeActor(_ => None), "bridge")
      implicit val timeout: akka.util.Timeout = 3.seconds
      implicit val sch: akka.actor.typed.Scheduler = sys.scheduler
      import akka.actor.typed.scaladsl.AskPattern._
      val fut = bridge.ask[BridgeReply](ref => Ask(2L, "ghost", "pb:x", Array.emptyByteArray, ref))
      Await.result(fut, 3.seconds) match {
        case ReplyErr(_, code, _) => assert(code == WireFrame.ErrCode.ActorNotFound)
        case other                => fail(s"unexpected: $other")
      }
    } finally sys.terminate()
  }

  test("stop is unsupported (I4 gap preserved)") {
    val sys = ActorSystem(Behaviors.empty[Any], "bridge-stop-test")
    try {
      val bridge = sys.systemActorOf(BridgeActor(_ => None), "bridge")
      implicit val timeout: akka.util.Timeout = 3.seconds
      implicit val sch: akka.actor.typed.Scheduler = sys.scheduler
      import akka.actor.typed.scaladsl.AskPattern._
      val fut = bridge.ask[StopAck](ref => StopAkka("x", ref))
      assert(Await.result(fut, 3.seconds) == StopUnsupported)
    } finally sys.terminate()
  }
}
