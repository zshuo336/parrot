package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.{AskPattern, Behaviors}
import io.netty.buffer.{ByteBuf, Unpooled}
import io.netty.channel.embedded.EmbeddedChannel
import io.netty.channel.{ChannelHandlerContext, ChannelOutboundHandlerAdapter, ChannelPromise}
import java.io.DataInputStream
import java.net.{Socket, ServerSocket}
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.{ConcurrentLinkedQueue, TimeUnit}
import org.scalatest.funsuite.AnyFunSuite
import java.util.concurrent.atomic.AtomicInteger
import scala.concurrent.Await
import scala.concurrent.duration._
import BridgeActor._
import WireFrame.FrameType._

class TerminalIngressSpec extends AnyFunSuite {
  class CloseGate(var blocked: Boolean) extends ChannelOutboundHandlerAdapter {
    private val waiting = scala.collection.mutable.ArrayBuffer.empty[(ChannelHandlerContext, ChannelPromise)]
    val requests = new AtomicInteger
    override def close(ctx: ChannelHandlerContext, promise: ChannelPromise): Unit = {
      if (blocked) waiting += ((ctx, promise)) else ctx.close(promise)
      requests.incrementAndGet(); ()
    }
    def release(): Unit = {
      blocked = false
      val pending = waiting.toVector; waiting.clear()
      pending.foreach { case (ctx, promise) => ctx.close(promise) }
    }
  }
  class Session(client: Boolean, delayedClose: Boolean = false) {
    val system = ActorSystem[Any](Behaviors.empty, "terminal-ingress")
    val asks = new AtomicInteger
    val tells = new AtomicInteger
    val commands = new AtomicInteger
    val held = new ConcurrentLinkedQueue[Ask]()
    val bridge = system.systemActorOf(Behaviors.receiveMessage[BridgeMsg] {
      case Ask(cid, _, "barrier", _, to) => to ! Replied(cid, "barrier", Array.emptyByteArray); Behaviors.same
      case request: Ask if request.typeKey == "hold" => asks.incrementAndGet(); held.add(request); Behaviors.same
      case Ask(cid, _, key, bytes, to) => asks.incrementAndGet();
        to ! Replied(cid, if (key == "bad-reply") 0xd800.toChar.toString else key, bytes); Behaviors.same
      case _: Tell => tells.incrementAndGet(); Behaviors.same
      case _ => Behaviors.same
    }, "bridge")
    val admin = system.systemActorOf(Behaviors.receiveMessage[AdminPort.Msg] {
      case _: AdminPort.CmdIn => commands.incrementAndGet(); Behaviors.same
      case AdminPort.Snapshot(to) => to ! Map.empty; Behaviors.same
      case _ => Behaviors.same
    }, "admin")
    val budget = GatewayAskBudget.owned(system)
    val handler = if (client) new ParrotClientHandler(bridge, "terminal", Some(admin), system.scheduler, budget)
      else new ParrotServerHandler(bridge, "terminal", Some(admin), system.scheduler, budget)
    val gate = new CloseGate(delayedClose)
    val channel = new EmbeddedChannel(gate, new WireDecoder(), new WireEncoder(), handler)
    if (client) { val hello = channel.readOutbound[ByteBuf](); assert(hello != null); hello.release() }
    channel.writeInbound(Unpooled.wrappedBuffer(frame(if (client) HANDSHAKE_ACK else HANDSHAKE,
      bytes = if (client) WireFrame.handshakeAckBody("peer") else WireFrame.handshakeBody("peer"))))
    if (!client) { val ack = channel.readOutbound[ByteBuf](); assert(ack != null); ack.release() }
    assert(channel.attr(WireFrame.HandshakeComplete).get() == java.lang.Boolean.TRUE)

    def check(a: Int, t: Int, c: Int): Unit = {
      import AskPattern._
      implicit val timeout: akka.util.Timeout = 3.seconds
      implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
      // writeInbound returned after posting every ingress effect. Both actor
      // barriers must run before checking absence, rather than relying on sleep.
      Await.result(bridge.ask[BridgeReply](to => Ask(900, "", "barrier", Array.emptyByteArray, to)), 4.seconds)
      Await.result(admin.ask[Map[String, AdminV2Executor.ComponentEntry]](AdminPort.Snapshot), 4.seconds)
      assert((asks.get(), tells.get(), commands.get()) == ((a, t, c)))
    }
    def close(): Unit = {
      gate.release(); channel.finishAndReleaseAll(); budget.close()
      system.terminate(); Await.result(system.whenTerminated, 10.seconds)
    }
  }
  def frame(kind: Byte, flags: Int = 0, bytes: Array[Byte] = Array.emptyByteArray): Array[Byte] =
    WireFrame.Frame(1, kind, flags, 7, 0, 8, "/user/target", "test", bytes).encode()
  def business(kind: Byte): Array[Byte] = kind match {
    case ASK => frame(ASK, bytes = WireFrame.withReplyToPrefix("/receipt", Array[Byte](42)))
    case SYSTEM_EVENT => frame(SYSTEM_EVENT, bytes = AdminV2Codec.encodeCommand(AdminV2Codec.AdminCommandV2.Stop(7, "/user/target")))
    case _ => frame(TELL, bytes = Array[Byte](42))
  }
  for (client <- Seq(false, true); kind <- Seq(ASK, TELL, SYSTEM_EVENT)) {
    test(s"coalesced frame $kind cannot execute after protocol rejection (client=$client)") {
      val s = new Session(client)
      val input = Unpooled.wrappedBuffer(frame(TELL, flags = 1) ++ business(kind))
      try {
        s.channel.writeInbound(input)
        assert(!s.channel.isActive)
        s.check(0, 0, 0)
        assert(input.refCnt() == 0)
      } finally { s.close(); if (input.refCnt() > 0) input.release(input.refCnt()) }
    }
  }
  for (client <- Seq(false, true)) {
    test(s"valid coalesced business traffic reaches all three dispatch paths (client=$client)") {
      val s = new Session(client)
      try {
        s.channel.writeInbound(Unpooled.wrappedBuffer(business(ASK) ++ business(TELL) ++ business(SYSTEM_EVENT)))
        s.check(1, 1, 1)
        assert(s.channel.isActive)
      } finally s.close()
    }
  }

  for (client <- Seq(false, true)) {
    test(s"already accepted work before a bad frame is preserved but its suffix is discarded (client=$client)") {
      val s = new Session(client)
      try {
        s.channel.writeInbound(Unpooled.wrappedBuffer(business(TELL) ++ frame(TELL, flags = 1) ++
          business(ASK) ++ business(TELL) ++ business(SYSTEM_EVENT)))
        s.check(0, 1, 0)
      } finally s.close()
    }
    for (prefix <- Seq("bad-ask-prefix", "unsupported-stop", "unknown-system-event", "route-hint")) {
      test(s"nonterminal $prefix keeps later coalesced dispatch usable (client=$client)") {
        val s = new Session(client)
        val head = prefix match {
          case "bad-ask-prefix" => frame(ASK, bytes = Array[Byte](0))
          case "unsupported-stop" => frame(STOP)
          case "unknown-system-event" => frame(SYSTEM_EVENT, bytes = Array[Byte](77))
          case _ => frame(ROUTE_HINT)
        }
        try {
          s.channel.writeInbound(Unpooled.wrappedBuffer(head ++ business(ASK) ++ business(TELL) ++ business(SYSTEM_EVENT)))
          s.check(1, 1, 1); assert(s.channel.isActive)
          if (prefix == "unsupported-stop") {
            val encoded = s.channel.readOutbound[ByteBuf]()
            assert(encoded != null, "unsupported STOP must return a rejection")
            try {
              val bytes = new Array[Byte](encoded.readableBytes()); encoded.readBytes(bytes)
              val rejection = WireFrame.decode(bytes).get._1
              assert(rejection.frameType == REPLY_ERR, "unsupported STOP must return recoverable REPLY_ERR")
              assert(rejection.correlationId == 7 && rejection.path.isEmpty && rejection.typeKey.isEmpty)
              assert(WireFrame.decodeErrPayload(rejection.payload) ==
                ((WireFrame.ErrCode.Forbidden, "akka stop unsupported (I4)")))
            } finally encoded.release()
          }
        } finally s.close()
      }
    }
    for (kind <- Seq(ASK, TELL, SYSTEM_EVENT)) {
      test(s"already decoded frame $kind is fenced even while physical close is delayed (client=$client)") {
        val s = new Session(client, delayedClose = true)
        val encoded = business(kind)
        val decoded = WireFrame.decode(encoded).get._1
        val buffer = Unpooled.wrappedBuffer(decoded.payload)
        val pending = new WireFrame.Frame(1, kind, 0, 7, 0, 8, decoded.path, decoded.typeKey, buffer)
        try {
          s.channel.writeInbound(Unpooled.wrappedBuffer(frame(TELL, flags = 1)))
          assert(s.channel.isActive, "close gate must hold physical retirement")
          s.channel.pipeline().context(classOf[WireDecoder]).fireChannelRead(pending)
          s.check(0, 0, 0)
          assert(buffer.refCnt() == 0)
          assert(s.channel.readOutbound[ByteBuf]() == null)
        } finally { s.close(); pending.releaseBuf() }
      }
    }
    for (cause <- Seq("duplicate-handshake", "invalid-version", "durable-envelope", "idle")) {
      test(s"terminal $cause discards queued suffix while close is pending (client=$client)") {
        val s = new Session(client, delayedClose = true)
        val invalid = cause match {
          case "duplicate-handshake" => frame(HANDSHAKE, bytes = WireFrame.handshakeBody("again"))
          case "invalid-version" => val bytes = business(TELL); bytes(4) = 2; bytes
          case "durable-envelope" => WireFrame.Frame(1, TELL, 0, 7, 0, 8, "/user/target", "parrot:durable#v1", Array[Byte](0)).encode()
          case _ => Array.emptyByteArray
        }
        try {
          if (cause == "idle") s.channel.pipeline().fireUserEventTriggered(io.netty.handler.timeout.IdleStateEvent.READER_IDLE_STATE_EVENT)
          val input = Unpooled.wrappedBuffer(invalid ++ business(TELL) ++ business(SYSTEM_EVENT))
          s.channel.writeInbound(input)
          s.check(0, 0, 0)
          assert(s.channel.isActive)
          assert(input.refCnt() == 0)
        } finally s.close()
      }
    }
    test(s"duplicate ASK fences later effects and its original late reply (client=$client)") {
      val s = new Session(client, delayedClose = true)
      try {
        val first = WireFrame.Frame(1, ASK, 0, 7, 0, 8, "/user/target", "hold",
          WireFrame.withReplyToPrefix("/receipt", Array[Byte](1)))
        s.channel.writeInbound(Unpooled.wrappedBuffer(first.encode()))
        s.check(1, 0, 0)
        s.channel.writeInbound(Unpooled.wrappedBuffer(business(ASK) ++ business(TELL) ++ business(SYSTEM_EVENT)))
        s.check(1, 0, 0)
        val admitted = s.held.poll(); assert(admitted != null)
        admitted.replyTo ! Replied(admitted.cid, "late", Array[Byte](9))
        val deadline = System.nanoTime() + 3.seconds.toNanos
        while (s.budget.snapshot._1 != 0 && System.nanoTime() < deadline) Thread.sleep(2)
        assert(s.budget.snapshot == (0, 0L))
        s.channel.runPendingTasks()
        assert(s.channel.readOutbound[ByteBuf]() == null, "late result escaped terminal fence")
      } finally s.close()
    }
  }
  test("encoder rejects late owned payloads after a delayed protocol close") {
    val s = new Session(false, delayedClose = true)
    val payload = Unpooled.buffer().writeByte(42)
    val late = new WireFrame.Frame(1, REPLY, 0, 7, 0, 8, "/receipt", "reply", payload)
    try {
      s.channel.writeInbound(Unpooled.wrappedBuffer(frame(TELL, flags = 1)))
      assert(s.channel.isActive)
      val future = s.channel.writeAndFlush(late)
      s.channel.runPendingTasks()
      assert(future.isDone && !future.isSuccess)
      assert(payload.refCnt() == 0 && s.channel.readOutbound[ByteBuf]() == null)
    } finally { s.close(); late.releaseBuf() }
  }

  class TcpPeer(val socket: Socket) {
    socket.setSoTimeout(5000)
    val input = new DataInputStream(socket.getInputStream)
    def send(bytes: Array[Byte]): Unit = { socket.getOutputStream.write(bytes); socket.getOutputStream.flush() }
    def read(): WireFrame.Frame = {
      val head = new Array[Byte](4); input.readFully(head)
      val size = ByteBuffer.wrap(head).order(ByteOrder.LITTLE_ENDIAN).getInt
      assert(size >= 28 && size <= WireFrame.MAX_FRAME_LEN)
      val body = new Array[Byte](size); input.readFully(body)
      WireFrame.decode(head ++ body).get._1
    }
  }
  for (registering <- Seq(false, true); kind <- Seq(ASK, TELL, SYSTEM_EVENT)) {
    test(s"real TCP coalesced rejection cannot dispatch frame $kind and a fresh connection recovers (registering=$registering)") {
      val s = new Session(false)
      val transport = new ParrotTransportExtension(s.system, s.bridge, "terminal-tcp", Some(s.admin))
      val hub = if (registering) new ServerSocket(0) else null
      val peers = scala.collection.mutable.ArrayBuffer.empty[TcpPeer]
      try {
        if (registering) { hub.setSoTimeout(5000); transport.registerTo("127.0.0.1", hub.getLocalPort) }
        else transport.listen(0)
        def connect(): TcpPeer = {
          val peer = new TcpPeer(if (registering) hub.accept() else new Socket("127.0.0.1", transport.port))
          peers += peer
          if (registering) { assert(peer.read().frameType == HANDSHAKE); peer.send(frame(HANDSHAKE_ACK, bytes = WireFrame.handshakeAckBody("hub"))) }
          else { peer.send(frame(HANDSHAKE, bytes = WireFrame.handshakeBody("peer"))); assert(peer.read().frameType == HANDSHAKE_ACK) }
          peer.send(frame(HEARTBEAT)); assert(peer.read().frameType == HEARTBEAT_ACK)
          peer
        }
        val bad = connect()
        bad.send(frame(TELL, flags = 1) ++ business(kind))
        assert(bad.input.read() == -1)
        // A fresh connection's wire barrier advances the same production Netty
        // event loop after the rejected read/decodeLast path, then both actors.
        val good = connect()
        good.send(WireFrame.Frame(1, ASK, 0, 99, 0, 8, "/user/target", "barrier",
          WireFrame.withReplyToPrefix("/receipt", Array.emptyByteArray)).encode())
        val barrier = good.read(); assert(barrier.frameType == REPLY && barrier.correlationId == 99)
        s.check(0, 0, 0)
        good.send(business(ASK)); assert(good.read().frameType == REPLY)
        s.check(1, 0, 0)
      } finally {
        peers.foreach(_.socket.close()); if (hub != null) hub.close()
        transport.shutdown().get(10, TimeUnit.SECONDS); s.close()
      }
    }
  }

  for (client <- Seq(false, true)) {
    test(s"async reply encoding failure fences later business before physical close (client=$client)") {
      val s = new Session(client, delayedClose = true)
      try {
        val request = WireFrame.Frame(1, ASK, 0, 7, 0, 8, "/user/target", "bad-reply",
          WireFrame.withReplyToPrefix("/receipt", Array[Byte](1)))
        s.channel.writeInbound(Unpooled.wrappedBuffer(request.encode()))
        val deadline = System.nanoTime() + 3.seconds.toNanos
        while (s.gate.requests.get() == 0 && System.nanoTime() < deadline) { s.channel.runPendingTasks(); Thread.sleep(2) }
        assert(s.gate.requests.get() > 0 && s.channel.isActive)
        s.channel.writeInbound(Unpooled.wrappedBuffer(business(TELL) ++ business(SYSTEM_EVENT)))
        s.check(1, 0, 0)
        assert(s.channel.readOutbound[ByteBuf]() == null)
      } finally s.close()
    }
  }
  test("a late successful ACK write cannot publish readiness or negotiated limits after rejection") {
    val close = new CloseGate(true)
    var held: ByteBuf = null
    var receipt: ChannelPromise = null
    val writes = new ChannelOutboundHandlerAdapter {
      override def write(ctx: ChannelHandlerContext, msg: Any, promise: ChannelPromise): Unit = {
        held = msg.asInstanceOf[ByteBuf]; receipt = promise
      }
    }
    val channel = new EmbeddedChannel(close, writes, new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "late-ack"))
    try {
      channel.writeInbound(Unpooled.wrappedBuffer(frame(HANDSHAKE, bytes = WireFrame.handshakeBody("peer"))))
      assert(receipt != null && channel.attr(WireFrame.HandshakeComplete).get() == null)
      channel.writeInbound(Unpooled.wrappedBuffer(frame(TELL, flags = 1)))
      assert(channel.isActive && close.requests.get() > 0)
      held.release(); held = null; receipt.setSuccess(); channel.runPendingTasks()
      assert(channel.attr(WireFrame.HandshakeComplete).get() == null)
      assert(channel.attr(WireFrame.PeerFrameLimit).get() == null)
    } finally { if (held != null) held.release(); close.release(); channel.finishAndReleaseAll() }
  }
  test("failed client HELLO write fences a later valid ACK while physical close is delayed") {
    val close = new CloseGate(true)
    val failedWrite = new ChannelOutboundHandlerAdapter {
      override def write(ctx: ChannelHandlerContext, msg: Any, promise: ChannelPromise): Unit = {
        io.netty.util.ReferenceCountUtil.release(msg)
        promise.setFailure(new java.io.IOException("injected HELLO write failure"))
      }
    }
    val channel = new EmbeddedChannel(close, failedWrite, new WireDecoder(), new WireEncoder(), new ParrotClientHandler(null, "failed-hello"))
    try {
      assert(channel.isActive && close.requests.get() > 0)
      channel.writeInbound(Unpooled.wrappedBuffer(frame(HANDSHAKE_ACK, bytes = WireFrame.handshakeAckBody("hub"))))
      assert(channel.attr(WireFrame.HandshakeComplete).get() == null)
      assert(channel.attr(WireFrame.PeerFrameLimit).get() == null)
    } finally { close.release(); channel.finishAndReleaseAll() }
  }
}
