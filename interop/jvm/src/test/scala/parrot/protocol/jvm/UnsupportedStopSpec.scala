package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.Behaviors
import io.netty.buffer.{ByteBuf, Unpooled}
import io.netty.channel.embedded.EmbeddedChannel
import java.io.{DataInputStream, EOFException}
import java.net.{ServerSocket, Socket}
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.TimeUnit
import org.scalatest.funsuite.AnyFunSuite
import scala.concurrent.Await
import scala.concurrent.duration._
import BridgeActor._
import WireFrame.FrameType._

class UnsupportedStopSpec extends AnyFunSuite {
  private val correlations = Seq(0L, 7L, Long.MaxValue, -1L)
  private def frame(kind: Byte, cid: Long = 7L, payload: Array[Byte] = Array.emptyByteArray,
                    flags: Int = 0): Array[Byte] =
    WireFrame.Frame(1, kind, flags, cid, 0, 8, "/user/live", "bin:ping", payload).encode()
  private def ask(cid: Long): Array[Byte] =
    frame(ASK, cid, WireFrame.withReplyToPrefix("/receipt", Array[Byte](39)))
  private def rejection(reply: WireFrame.Frame, cid: Long): Unit = {
    assert(reply.frameType == REPLY_ERR, "unsupported STOP must return recoverable REPLY_ERR")
    assert(reply.correlationId == cid, "STOP rejection must preserve the unsigned wire correlation")
    assert(reply.flags == 0 && reply.path.isEmpty && reply.typeKey.isEmpty)
    assert(WireFrame.decodeErrPayload(reply.payload) ==
      ((WireFrame.ErrCode.Forbidden, "akka stop unsupported (I4)")))
  }
  private def pong(reply: WireFrame.Frame, cid: Long): Unit = {
    assert(reply.frameType == REPLY && reply.correlationId == cid && reply.typeKey == "bin:pong")
    assert(reply.path == "/receipt" && reply.payload.sameElements(Array[Byte](42)))
  }
  private class Owner {
    val system = ActorSystem[Nothing](Behaviors.empty, "unsupported-stop")
    val target = system.systemActorOf(Behaviors.receiveMessage[Any] {
      case BridgeAsk("bin:ping", bytes, to) =>
        assert(bytes.sameElements(Array[Byte](39)))
        to ! BridgeReplyOk("bin:pong", Array[Byte](42)); Behaviors.same
      case _ => Behaviors.same
    }, "live")
    val bridge = system.systemActorOf(BridgeActor(path => if (path == "live") Some(target) else None), "bridge")
    val budget = GatewayAskBudget.owned(system)
    def close(): Unit = {
      budget.close(); system.terminate(); Await.result(system.whenTerminated, 10.seconds)
    }
  }
  private class Embedded(client: Boolean) {
    val owner = new Owner
    private val handler = if (client)
      new ParrotClientHandler(owner.bridge, "jvm", None, owner.system.scheduler, owner.budget)
    else new ParrotServerHandler(owner.bridge, "jvm", None, owner.system.scheduler, owner.budget)
    val channel = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), handler)
    def read(): WireFrame.Frame = {
      val deadline = System.nanoTime() + 5.seconds.toNanos
      var buffer: ByteBuf = null
      while (buffer == null && channel.isActive && System.nanoTime() < deadline) {
        channel.runPendingTasks(); buffer = channel.readOutbound[ByteBuf]()
        if (buffer == null) Thread.sleep(1)
      }
      assert(buffer != null, "gateway must provide the expected rejection or business reply")
      try {
        val bytes = new Array[Byte](buffer.readableBytes()); buffer.readBytes(bytes)
        WireFrame.decode(bytes).get._1
      } finally buffer.release()
    }
    if (client) assert(read().frameType == HANDSHAKE)
    channel.writeInbound(Unpooled.wrappedBuffer(frame(if (client) HANDSHAKE_ACK else HANDSHAKE,
      payload = if (client) WireFrame.handshakeAckBody("hub") else WireFrame.handshakeBody("peer"))))
    if (!client) assert(read().frameType == HANDSHAKE_ACK)
    def close(): Unit = { channel.finishAndReleaseAll(); owner.close() }
  }
  private class Tcp(registering: Boolean) {
    val owner = new Owner
    val transport = new ParrotTransportExtension(owner.system, owner.bridge, "jvm")
    val listener = if (registering) new ServerSocket(0) else null
    if (registering) {
      listener.setSoTimeout(5000); transport.registerTo("127.0.0.1", listener.getLocalPort)
    } else transport.listen(0)
    val socket = if (registering) listener.accept() else new Socket("127.0.0.1", transport.port)
    socket.setSoTimeout(5000)
    val input = new DataInputStream(socket.getInputStream)
    def send(bytes: Array[Byte]): Unit = { socket.getOutputStream.write(bytes); socket.getOutputStream.flush() }
    def read(): WireFrame.Frame = {
      val header = new Array[Byte](4)
      try input.readFully(header) catch {
        case _: EOFException => fail("recoverable STOP rejection must not close the TCP connection")
      }
      val size = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN).getInt
      assert(size >= 28 && size <= WireFrame.MAX_FRAME_LEN)
      val body = new Array[Byte](size); input.readFully(body)
      WireFrame.decode(header ++ body).get._1
    }
    if (registering) {
      assert(read().frameType == HANDSHAKE)
      send(frame(HANDSHAKE_ACK, payload = WireFrame.handshakeAckBody("hub")))
    } else {
      send(frame(HANDSHAKE, payload = WireFrame.handshakeBody("peer")))
      assert(read().frameType == HANDSHAKE_ACK)
    }
    def close(): Unit = {
      socket.close(); if (listener != null) listener.close()
      transport.shutdown().get(10, TimeUnit.SECONDS); owner.close()
    }
  }

  for (client <- Seq(false, true)) {
    test(s"STOP rejection has a recoverable envelope and target remains usable (client=$client)") {
      val s = new Embedded(client)
      try for (cid <- correlations) {
        s.channel.writeInbound(Unpooled.wrappedBuffer(frame(STOP, cid) ++ frame(HEARTBEAT)))
        rejection(s.read(), cid)
        assert(s.read().frameType == HEARTBEAT_ACK)
        assert(s.channel.isActive, "recoverable STOP rejection must keep the channel active")
        s.channel.writeInbound(Unpooled.wrappedBuffer(ask(99))); pong(s.read(), 99)
      } finally s.close()
    }
    test(s"received terminal ERROR discards coalesced business and owns inbound buffers (client=$client)") {
      val inputs = Seq(Array.emptyByteArray, Array[Byte](0), Array[Byte](12, 0, 0, 0, -1))
      for (payload <- inputs; flags <- Seq(0, 8, 0xffff)) {
        val s = new Embedded(client)
        val encoded = Unpooled.wrappedBuffer(frame(ERROR, payload = payload, flags = flags) ++ ask(99) ++ frame(HEARTBEAT))
        try {
          s.channel.writeInbound(encoded)
          assert(!s.channel.isActive, "received protocol ERROR must retire the connection")
          assert(encoded.refCnt() == 0)
          assert(s.channel.readOutbound[ByteBuf]() == null, "terminal ERROR must not emit a response or suffix reply")
          assert(s.owner.budget.snapshot == (0, 0L))
        } finally { s.close(); if (encoded.refCnt() > 0) encoded.release(encoded.refCnt()) }
      }
    }
  }
  for (registering <- Seq(false, true)) {
    test(s"real TCP STOP rejection preserves correlation, target and following frames (registering=$registering)") {
      val s = new Tcp(registering)
      try for (cid <- correlations) {
        s.send(frame(STOP, cid) ++ frame(HEARTBEAT))
        rejection(s.read(), cid)
        assert(s.read().frameType == HEARTBEAT_ACK)
        s.send(ask(99)); pong(s.read(), 99)
      } finally s.close()
    }
    test(s"real TCP received ERROR closes without a reply or coalesced heartbeat (registering=$registering)") {
      val s = new Tcp(registering)
      try {
        s.send(frame(ERROR, payload = Array[Byte](12, 0, 0, 0, -1)) ++ frame(HEARTBEAT))
        assert(s.input.read() == -1, "received protocol ERROR must retire TCP without replying")
      } finally s.close()
    }
  }
}
