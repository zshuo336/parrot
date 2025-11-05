package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.Behaviors
import org.scalatest.funsuite.AnyFunSuite
import java.io.DataInputStream
import java.net.{Socket, ServerSocket}
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.{ConcurrentLinkedQueue, CountDownLatch, TimeUnit}
import scala.concurrent.Await
import scala.concurrent.duration._
import BridgeActor._
import WireFrame.FrameType._

class GatewayAskAdmissionSpec extends AnyFunSuite {
  class Session(limits: GatewayAskLimits = GatewayAskLimits(), registering: Boolean = false) {
    val held = new ConcurrentLinkedQueue[Ask]()
    val barriers = new ConcurrentLinkedQueue[CountDownLatch]()
    val system = ActorSystem[Any](Behaviors.empty, "ask-admission")
    val bridge = system.systemActorOf(Behaviors.receiveMessage[BridgeMsg] {
      case request: Ask =>
        if (request.typeKey == "hold") held.add(request)
        else if (request.typeKey == "wrong-error-cid") request.replyTo ! ReplyErr(999, WireFrame.ErrCode.CodecError, "rejected")
        else request.replyTo ! Replied(if (request.typeKey == "wrong-cid") 999 else request.cid,
          request.typeKey, request.payload)
        Behaviors.same
      case _: Tell => barriers.poll().countDown(); Behaviors.same
      case _ => Behaviors.same
    }, "bridge")
    val transport = new ParrotTransportExtension(system, bridge, "admission", None, limits)
    val hub = if (registering) new ServerSocket(0) else null
    if (registering) { hub.setSoTimeout(4000); transport.registerTo("127.0.0.1", hub.getLocalPort) }
    else transport.listen(0)
    val peers = scala.collection.mutable.ArrayBuffer.empty[Peer]
    class Peer {
      val socket = if (registering) hub.accept() else new Socket("127.0.0.1", transport.port)
      socket.setSoTimeout(4000)
      val input = new DataInputStream(socket.getInputStream)
      def send(kind: Byte, cid: Long = 0, key: String = "", payload: Array[Byte] = Array.emptyByteArray): Unit = {
        socket.getOutputStream.write(WireFrame.Frame(1, kind, 0, cid, 0, 8, "/user/target", key, payload).encode())
        socket.getOutputStream.flush()
      }
      def ask(cid: Long, payload: Array[Byte] = Array.emptyByteArray, key: String = "hold", reply: String = "/receipt"): Unit =
        send(ASK, cid, key, WireFrame.withReplyToPrefix(reply, payload))
      def read(): WireFrame.Frame = {
        val prefix = new Array[Byte](4); input.readFully(prefix)
        val size = ByteBuffer.wrap(prefix).order(ByteOrder.LITTLE_ENDIAN).getInt
        assert(size >= 28 && size <= WireFrame.MAX_FRAME_LEN)
        val body = new Array[Byte](size); input.readFully(body)
        WireFrame.decode(prefix ++ body).get._1
      }
      if (registering) {
        assert(read().frameType == HANDSHAKE)
        send(HANDSHAKE_ACK, payload = WireFrame.handshakeAckBody("hub"))
        send(HEARTBEAT); assert(read().frameType == HEARTBEAT_ACK)
      } else {
        send(HANDSHAKE, payload = WireFrame.handshakeBody("peer"))
        assert(read().frameType == HANDSHAKE_ACK)
      }
      def barrier(): Unit = {
        val done = new CountDownLatch(1); barriers.add(done); send(TELL)
        assert(done.await(3, TimeUnit.SECONDS), "bridge admission barrier")
      }
    }
    def peer(): Peer = { val value = new Peer; peers += value; value }
    def close(): Unit = {
      peers.foreach(_.socket.close())
      if (hub != null) hub.close()
      transport.shutdown().get(10, TimeUnit.SECONDS)
      system.terminate(); Await.result(system.whenTerminated, 10.seconds)
    }
  }
  def session(run: Session => Unit): Unit = {
    session(GatewayAskLimits())(run)
  }
  def session(limits: GatewayAskLimits, registering: Boolean = false)(run: Session => Unit): Unit = {
    val value = new Session(limits, registering)
    try run(value) finally value.close()
  }
  def eventually(condition: => Boolean, waitFor: FiniteDuration = 3.seconds): Unit = {
    val deadline = System.nanoTime() + waitFor.toNanos
    while (!condition && System.nanoTime() < deadline) Thread.sleep(2)
    assert(condition)
  }
  private def overloaded(frame: WireFrame.Frame, cid: Long): Unit = {
    assert(frame.frameType == REPLY_ERR, s"missing overload receipt: $frame")
    assert(frame.correlationId == cid && frame.path == "/receipt")
    assert(WireFrame.decodeErrPayload(frame.payload)._1 == WireFrame.ErrCode.Overloaded)
  }
  test("real TCP rejects the 129th outstanding ASK before bridge admission and keeps heartbeat alive") {
    session { s =>
      val p = s.peer()
      (1L to 129L).foreach(id => p.ask(id))
      p.barrier(); p.send(HEARTBEAT)
      overloaded(p.read(), 129)
      assert(p.read().frameType == HEARTBEAT_ACK)
      assert(s.held.size() == 128)
    }
  }
  test("real TCP bounds outstanding request bytes independently of request count") {
    session { s =>
      val p = s.peer()
      val bytes = new Array[Byte](9 * 1024 * 1024)
      p.ask(1, bytes); p.ask(2, bytes); p.barrier(); p.send(HEARTBEAT)
      overloaded(p.read(), 2)
      assert(p.read().frameType == HEARTBEAT_ACK)
      assert(s.held.size() == 1)
    }
  }
  test("the original wire correlation owns the receipt even if an embedded bridge returns another id") {
    session { s =>
      val p = s.peer(); p.ask(7, Array[Byte](42), "wrong-cid")
      val reply = p.read()
      assert(reply.frameType == REPLY && reply.correlationId == 7 && reply.path == "/receipt")
      assert(reply.payload.sameElements(Array[Byte](42)))
    }
  }

  test("transport-wide count is shared across sockets and successful completion restores capacity") {
    session(GatewayAskLimits(3, 4096, 2, 2048)) { s =>
      val a = s.peer(); val b = s.peer()
      a.ask(1); a.ask(2); a.barrier()
      b.ask(1); b.ask(2); b.barrier()
      overloaded(b.read(), 2)
      assert(s.held.size() == 3 && s.transport.pendingAsks._1 == 3)
      val first = s.held.poll(); first.replyTo ! Replied(first.cid, "ok", Array[Byte](1))
      assert(a.read().correlationId == 1)
      eventually(s.transport.pendingAsks._1 == 2)
      b.ask(3, Array[Byte](3), "echo")
      assert(b.read().payload.sameElements(Array[Byte](3)))
      eventually(s.transport.pendingAsks._1 == 2)
    }
  }
  test("transport-wide bytes reject another socket before either connection's own budget is full") {
    session(GatewayAskLimits(10, 200, 5, 150)) { s =>
      val a = s.peer(); val b = s.peer()
      a.ask(1, new Array[Byte](84)); a.barrier() // 84 + 2*(target + hold + /receipt) = 120
      assert(s.transport.pendingAsks == (1, 120L))
      b.ask(1, new Array[Byte](84)); overloaded(b.read(), 1)
      val first = s.held.poll(); first.replyTo ! ReplyErr(first.cid, WireFrame.ErrCode.CodecError, "rejected")
      assert(a.read().frameType == REPLY_ERR)
      eventually(s.transport.pendingAsks == (0, 0L))
      b.ask(1, new Array[Byte](84), "echo")
      assert(b.read().frameType == REPLY)
    }
  }
  test("metadata bytes and the exact per-connection byte boundary are accounted for") {
    session(GatewayAskLimits(10, 100, 10, 100)) { s =>
      val p = s.peer()
      p.ask(1, new Array[Byte](64)); p.barrier()
      assert(s.transport.pendingAsks == (1, 100L))
      p.ask(2); overloaded(p.read(), 2)
      val first = s.held.poll(); first.replyTo ! Replied(first.cid, "ok", Array.emptyByteArray)
      assert(p.read().frameType == REPLY)
      eventually(s.transport.pendingAsks == (0, 0L))
      val replyPath = "/" + "界" * 48
      p.ask(3, reply = replyPath)
      val rejected = p.read()
      assert(rejected.path == replyPath && rejected.correlationId == 3)
      assert(WireFrame.decodeErrPayload(rejected.payload)._1 == WireFrame.ErrCode.Overloaded)
      p.barrier(); assert(s.held.isEmpty)
    }
  }
  test("malformed ASK stays a protocol error while capacity is exhausted") {
    session(GatewayAskLimits(1, 1024, 1, 1024)) { s =>
      val p = s.peer(); p.ask(1); p.barrier()
      p.send(ASK, 2, "echo", Array[Byte](0))
      val malformed = p.read()
      assert(malformed.correlationId == 2 && malformed.path == "")
      assert(WireFrame.decodeErrPayload(malformed.payload)._1 == WireFrame.ErrCode.ProtocolViolation)
      p.ask(3); overloaded(p.read(), 3)
      assert(s.transport.pendingAsks._1 == 1)
    }
  }
  for (malformed <- Seq(false, true)) {
    test(s"duplicate pending correlation closes its socket without a conflicting receipt (malformed=$malformed)") {
      session { s =>
        val p = s.peer(); p.ask(1); p.barrier()
        if (malformed) p.send(ASK, 1, "echo", Array[Byte](0)) else p.ask(1, key = "echo")
        assert(p.input.read() == -1)
        assert(s.held.size() == 1)
        val first = s.held.poll(); first.replyTo ! Replied(1, "old", Array.emptyByteArray)
        eventually(s.transport.pendingAsks == (0, 0L))
        val q = s.peer(); q.ask(1, key = "echo"); assert(q.read().frameType == REPLY)
      }
    }
  }
  for (registering <- Seq(false, true)) {
    test(s"disconnect and reconnect cannot bypass live ASK capacity or redirect a late reply (registering=$registering)") {
      session(GatewayAskLimits(1, 1024, 1, 1024), registering) { s =>
        val a = s.peer(); a.ask(7); a.barrier(); a.socket.close()
        val b = s.peer(); b.ask(7, key = "echo"); overloaded(b.read(), 7)
        assert(s.transport.pendingAsks._1 == 1)
        val first = s.held.poll(); first.replyTo ! Replied(7, "old", Array[Byte](1))
        eventually(s.transport.pendingAsks == (0, 0L))
        b.ask(7, Array[Byte](2), "echo")
        val next = b.read()
        assert(next.correlationId == 7 && next.typeKey == "echo" && next.payload.sameElements(Array[Byte](2)))
        first.replyTo ! Replied(7, "old-again", Array[Byte](1))
        b.send(HEARTBEAT); assert(b.read().frameType == HEARTBEAT_ACK)
      }
    }
  }
  test("outer timeout releases capacity and fences an old result before same-id reuse") {
    session(GatewayAskLimits(1, 1024, 1, 1024)) { s =>
      val p = s.peer(); p.socket.setSoTimeout(8000); p.ask(1); p.barrier()
      val failed = p.read()
      assert(failed.frameType == REPLY_ERR && failed.correlationId == 1)
      assert(WireFrame.decodeErrPayload(failed.payload)._1 == WireFrame.ErrCode.Timeout)
      eventually(s.transport.pendingAsks == (0, 0L))
      val first = s.held.poll(); first.replyTo ! Replied(1, "late", Array.emptyByteArray)
      p.ask(1, Array[Byte](3), "echo")
      val reply = p.read(); assert(reply.typeKey == "echo" && reply.payload.sameElements(Array[Byte](3)))
    }
  }
  test("business error correlation is owned by the wire request and both ids can be reused") {
    session { s =>
      val p = s.peer(); p.ask(7, key = "wrong-error-cid")
      val rejected = p.read()
      assert(rejected.frameType == REPLY_ERR && rejected.correlationId == 7)
      assert(WireFrame.decodeErrPayload(rejected.payload)._1 == WireFrame.ErrCode.CodecError)
      for (id <- Seq(7L, 999L)) { p.ask(id, key = "echo"); assert(p.read().correlationId == id) }
    }
  }

  test("transport shutdown suppresses a pending receipt and completion still releases its budget") {
    session { s =>
      val p = s.peer(); p.ask(1); p.barrier()
      s.transport.shutdown().get(10, TimeUnit.SECONDS)
      assert(p.input.read() == -1)
      assert(s.transport.pendingAsks._1 == 1)
      val first = s.held.poll(); first.replyTo ! Replied(1, "retired", Array.emptyByteArray)
      eventually(s.transport.pendingAsks == (0, 0L))
    }
  }
  test("ActorSystem retirement terminates the bounded waiters rather than leaking their permits") {
    session { s =>
      val p = s.peer(); p.ask(1); p.barrier()
      s.system.terminate(); Await.result(s.system.whenTerminated, 10.seconds)
      eventually(s.transport.pendingAsks == (0, 0L), 8.seconds)
      assert(p.input.read() == -1)
    }
  }

  test("direct handlers bind an ActorSystem budget and reject a scheduler without lifecycle ownership") {
    session { s =>
      intercept[IllegalArgumentException](new ParrotServerHandler(s.bridge, "direct", None, s.system.scheduler))
      intercept[IllegalArgumentException](new ParrotClientHandler(s.bridge, "direct", None, s.system.scheduler))
      assert(new ParrotServerHandler(s.bridge, "direct", None, s.system) != null)
      assert(new ParrotClientHandler(s.bridge, "direct", None, s.system) != null)
      val shared = GatewayAskBudget.owned(s.system)
      assert(new ParrotServerHandler(s.bridge, "shared", None, s.system.scheduler, shared) != null)
      val request = shared.connection().acquire(1, 10).toOption.get
      s.system.terminate(); Await.result(s.system.whenTerminated, 10.seconds)
      eventually(shared.snapshot == (0, 0L))
      request.complete(fail("direct handler's retired budget delivered a response"))
    }
  }
}
