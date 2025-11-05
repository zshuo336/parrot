package parrot.protocol.jvm

import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorRef, ActorSystem, Behavior}
import BridgeActor._
import org.scalatest.funsuite.AnyFunSuite
import scala.concurrent.Await
import scala.concurrent.duration._

/** 桥语义集成：echo（ASK→REPLY）/ cpu（载荷计算）/ tell / stop-unsupported。 */
class BridgeEchoSpec extends AnyFunSuite {
  test("deployment paths and legacy aliases resolve to the same actor") {
    Seq("/user/alpha-0", "/jvm/user/alpha-0", "parrot://gateway/user/alpha-0", "parrot://gateway/jvm/user/alpha-0").foreach { path =>
      assert(ParrotServerHandler.actorPath(path) == "alpha-0")
    }
    assert(ParrotServerHandler.actorPath("/userland/alpha") == "/userland/alpha")
    assert(ParrotServerHandler.actorPath("parrot://gateway") == "")
  }


  test("component child resolution preserves order and cannot escape the owned root") {
    val system = ActorSystem(Behaviors.empty[Any], "bridge-children-test")
    val name = "owned-child-test"
    try {
      val wrapper = system.systemActorOf(Behaviors.setup[Any] { outer =>
        outer.spawn(Behaviors.setup[Any] { impl =>
          impl.spawn(Behaviors.setup[Any] { _ =>
            var values = Vector.empty[Byte]
            Behaviors.receiveMessage[Any] {
              case BridgeTell(_, payload) => values ++= payload.toVector; Behaviors.same
              case BridgeAsk(_, _, replyTo) => replyTo ! BridgeReplyOk("test:state", values.toArray); Behaviors.same
              case _ => Behaviors.same
            }
          }, "lifecycle")
          Behaviors.empty[Any]
        }, "impl")
        Behaviors.empty[Any]
      }, name)
      AdminV2Executor.ComponentRoutes(system).put(name, wrapper)
      val bridge = system.systemActorOf(BridgeActor(
        AdminV2Executor.ComponentRoutes(system).get,
        path => AdminV2Executor.ComponentRoutes(system).resolveChild(path)(system.executionContext)
      ), "bridge")
      implicit val timeout: akka.util.Timeout = 4.seconds
      implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
      import akka.actor.typed.scaladsl.AskPattern._
      (1 to 8).foreach(i => bridge ! Tell(s"$name/lifecycle", "test:state", Array(i.toByte)))
      val result = Await.result(bridge.ask[BridgeReply](ref => Ask(9, s"$name/lifecycle", "test:state", Array.emptyByteArray, ref)), 5.seconds)
      result match {
        case Replied(9, _, payload) => assert(payload.toVector == (1 to 8).map(_.toByte).toVector)
        case other => fail(s"unexpected child response: $other")
      }
      Seq(s"$name/../bridge", s"$name/*", s"$name//lifecycle", s"${name}suffix/lifecycle").foreach { path =>
        Await.result(bridge.ask[BridgeReply](ref => Ask(10, path, "test", Array.emptyByteArray, ref)), 5.seconds) match {
          case ReplyErr(10, WireFrame.ErrCode.ActorNotFound, _) => ()
          case other => fail(s"path escaped root: $path $other")
        }
      }
    } finally {
      AdminV2Executor.ComponentRoutes(system).remove(name)
      system.terminate()
      Await.result(system.whenTerminated, 5.seconds)
    }
  }

  test("pending child resolution is bounded and does not block existing actors") {
    val system = ActorSystem(Behaviors.empty[Any], "bridge-resolve-cap-test")
    try {
      val echo = system.systemActorOf(echoAny, "echo")
      val resolved = scala.concurrent.Promise[Option[ActorRef[Any]]]()
      val bridge = system.systemActorOf(BridgeActor(path => if (path == "echo") Some(echo) else None, _ => resolved.future), "bridge")
      implicit val timeout: akka.util.Timeout = 5.seconds
      implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
      import akka.actor.typed.scaladsl.AskPattern._
      val pending = (0 until 1024).map(i => bridge.ask[BridgeReply](ref => Ask(i, "pending/child", "test", Array.emptyByteArray, ref)))
      Await.result(bridge.ask[BridgeReply](ref => Ask(1024, "pending/child", "test", Array.emptyByteArray, ref)), 3.seconds) match {
        case ReplyErr(1024, WireFrame.ErrCode.Overloaded, _) => ()
        case other => fail(s"unbounded child resolution: $other")
      }
      assert(Await.result(bridge.ask[BridgeReply](ref => Ask(1025, "echo", "test", Array.emptyByteArray, ref)), 3.seconds).isInstanceOf[Replied])
      resolved.success(Some(echo))
      pending.foreach(future => assert(Await.result(future, 5.seconds).isInstanceOf[Replied]))
    } finally {
      system.terminate()
      Await.result(system.whenTerminated, 5.seconds)
    }
  }

  def echoAny: Behavior[Any] = Behaviors.receiveMessage {
    case BridgeAsk(_, payload, replyTo) =>
      replyTo ! BridgeReplyOk("pb:parrot.protocol.v1.AkkaEnvelope", payload)
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

  test("production CPU actor obeys Rust bincode vectors and remains alive after malformed requests") {
    val sys = ActorSystem(Behaviors.empty[Any], "bridge-cpu-test")
    try {
      val cpu = sys.systemActorOf(ParrotGatewayMain.cpuAny, "cpu")
      val bridge = sys.systemActorOf(BridgeActor(_ => Some(cpu)), "bridge")
      implicit val timeout: akka.util.Timeout = 3.seconds
      implicit val sch: akka.actor.typed.Scheduler = sys.scheduler
      import akka.actor.typed.scaladsl.AskPattern._
      def hex(text: String): Array[Byte] = text.grouped(2).map(Integer.parseInt(_,16).toByte).toArray
      val rows = scala.util.Using.resource(scala.io.Source.fromFile("../../docs/vectors/jvm-codecs.tsv")) { source =>
        source.getLines().filterNot(_.startsWith("#")).map(_.split("\t",-1)).filter(_(0)=="cpu").toVector
      }
      Await.result(bridge.ask[BridgeReply](ref => Ask(999,"cpu","pb:wrong",Array[Byte](41),ref)),3.seconds) match {
        case ReplyErr(999,WireFrame.ErrCode.UnknownTypeKey,_) => ()
        case other => fail(s"wrong CPU type key accepted: $other")
      }
      (rows :+ Array("cpu","after-errors","29","2a")).zipWithIndex.foreach { case(row,index) =>
        val Array(_,name,input,expected)=row
        val result=Await.result(bridge.ask[BridgeReply](ref => Ask(index.toLong,"cpu","bin:parrot.interop.Cpu#v1",hex(input),ref)),3.seconds)
        result match {
          case Replied(cid,key,payload) =>
            assert(cid==index.toLong && key=="bin:parrot.interop.CpuResult#v1")
            assert(expected!="ERR" && payload.sameElements(hex(expected)),name)
          case ReplyErr(cid,code,_) => assert(cid==index.toLong && expected=="ERR" && code==WireFrame.ErrCode.CodecError,name)
          case other => fail(s"unexpected $name: $other")
        }
      }
    } finally {
      sys.terminate()
      Await.result(sys.whenTerminated,5.seconds)
    }
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
