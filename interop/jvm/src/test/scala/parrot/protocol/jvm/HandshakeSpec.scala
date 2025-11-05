package parrot.protocol.jvm

import io.netty.buffer.{ByteBuf, Unpooled}
import io.netty.channel.embedded.EmbeddedChannel
import org.scalatest.funsuite.AnyFunSuite

class HandshakeSpec extends AnyFunSuite {
  private def frame(ft: Byte, body: Array[Byte]) = WireFrame.Frame(1, ft, 0, 1, 0, 8, "", "", body)
  private def tlv(tag: Int, bytes: Array[Byte]) = Array(tag.toByte, bytes.length.toByte, (bytes.length >> 8).toByte) ++ bytes

  test("server does not accept malformed or missing handshake fields") {
    val valid = WireFrame.handshakeBody("peer")
    val bad = Seq(Array.emptyByteArray, valid.dropRight(1), valid ++ Array[Byte](42),
      valid ++ tlv(1, Array[Byte](112)), valid ++ tlv(9, Array[Byte](112, 98)))
    for (body <- bad) {
      val ch = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "server"))
      try {
        ch.writeInbound(Unpooled.wrappedBuffer(frame(WireFrame.FrameType.HANDSHAKE, body).encode()))
        assert(!ch.isActive, "malformed handshake accepted")
        assert(ch.readOutbound[ByteBuf]() == null)
      } finally ch.finishAndReleaseAll()
    }
  }

  test("server emits chosen codec tag9 while preserving direct-address tag8") {
    val ch = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "server"))
    try {
      ch.writeInbound(Unpooled.wrappedBuffer(frame(WireFrame.FrameType.HANDSHAKE, WireFrame.handshakeBody("peer")).encode()))
      val ack = ch.readOutbound[ByteBuf]()
      try {
        val bytes = new Array[Byte](ack.readableBytes()); ack.readBytes(bytes)
        val tags = WireFrame.parseTlv(WireFrame.decode(bytes).get._1.payload).toMap
        assert(tags.get(9.toByte).exists(_.sameElements("bin".getBytes("UTF-8"))))
        assert(!tags.contains(8.toByte))
      } finally ack.release()
    } finally ch.finishAndReleaseAll()
  }

  test("connection gauges only include completed handshakes and retire clients without underflow") {
    val start = GatewayMetrics.snapshot()
    val server = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "server"))
    try assert(GatewayMetrics.snapshot().connections == start.connections)
    finally server.finishAndReleaseAll()
    assert(GatewayMetrics.snapshot().handshakesFailed == start.handshakesFailed + 1)
    val beforeClient = GatewayMetrics.snapshot()
    val client = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotClientHandler(null, "client"))
    client.finishAndReleaseAll()
    assert(GatewayMetrics.snapshot().connections == beforeClient.connections)
    assert(GatewayMetrics.snapshot().handshakesFailed == beforeClient.handshakesFailed + 1)
  }

  test("ACK with no valid identity does not mark the client ready") {
    val ch = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotClientHandler(null, "client"))
    try {
      val hello = ch.readOutbound[ByteBuf](); hello.release()
      ch.writeInbound(Unpooled.wrappedBuffer(frame(WireFrame.FrameType.HANDSHAKE_ACK, Array.emptyByteArray).encode()))
      assert(!ch.isActive)
    } finally ch.finishAndReleaseAll()
  }

  test("handshake success waits for the actual ACK write and failure retires once") {
    for (success <- Seq(true, false)) {
      var held: ByteBuf = null
      var receipt: io.netty.channel.ChannelPromise = null
      val gate = new io.netty.channel.ChannelOutboundHandlerAdapter {
        override def write(ctx: io.netty.channel.ChannelHandlerContext, msg: Any, promise: io.netty.channel.ChannelPromise): Unit = {
          held = msg.asInstanceOf[ByteBuf]; receipt = promise
        }
      }
      val before = GatewayMetrics.snapshot()
      val ch = new EmbeddedChannel(gate, new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "server"))
      val body = WireFrame.parseTlv(WireFrame.handshakeBody("peer")).flatMap { case (tag, value) =>
        tlv(tag, if (tag == 5) Array[Byte](28, 0, 0, 0) else value)
      }.toArray
      val hello = frame(WireFrame.FrameType.HANDSHAKE, body).encode()
      try {
        ch.writeInbound(Unpooled.wrappedBuffer(hello))
        assert(receipt != null)
        assert(ch.attr(WireFrame.PeerFrameLimit).get() == null, "limit activated before ACK completion")
        val ackBytes = held.readableBytes()
        val pending = GatewayMetrics.snapshot()
        assert(pending.connections == before.connections && pending.handshakesOk == before.handshakesOk)
        assert(pending.bytesTx == before.bytesTx)
        assert(pending.bytesRx - before.bytesRx == hello.length)
        held.release(); held = null
        if (success) receipt.setSuccess() else receipt.setFailure(new java.io.IOException("injected partial write"))
        ch.runPendingTasks()
        val after = GatewayMetrics.snapshot()
        if (success) assert(ch.attr(WireFrame.PeerFrameLimit).get().longValue() == 28)
        else assert(ch.attr(WireFrame.PeerFrameLimit).get() == null)
        assert(after.bytesTx - before.bytesTx == (if (success) ackBytes else 0))
        assert(after.connections - before.connections == (if (success) 1 else 0))
        assert(after.handshakesOk - before.handshakesOk == (if (success) 1 else 0))
        ch.finishAndReleaseAll()
        ch.pipeline().fireChannelInactive() // duplicate notification must not retire twice
        assert(GatewayMetrics.snapshot().connections == before.connections)
        assert(GatewayMetrics.snapshot().handshakesFailed - before.handshakesFailed == (if (success) 0 else 1))
      } finally {
        if (held != null) held.release()
        ch.finishAndReleaseAll()
      }
    }
  }

  test("handshake has a total deadline even if TCP stays connected") {
    val before = GatewayMetrics.snapshot()
    val ch = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "server"))
    try {
      ch.advanceTimeBy(11, java.util.concurrent.TimeUnit.SECONDS)
      ch.runScheduledPendingTasks()
      assert(!ch.isActive)
      assert(GatewayMetrics.snapshot().handshakesFailed == before.handshakesFailed + 1)
      assert(GatewayMetrics.snapshot().connections == before.connections)
    } finally ch.finishAndReleaseAll()
  }

  test("complete Wire RX and successful TX include UTF-8 metadata and errors") {
    val before = GatewayMetrics.snapshot()
    val ch = new EmbeddedChannel(new WireDecoder(), new WireEncoder())
    val ask = WireFrame.Frame(1, WireFrame.FrameType.ASK, 0, 3, 0, 8, "/目录/🙂", "bin:键", Array[Byte](1, 2)).encode()
    val reply = WireFrame.Frame(1, WireFrame.FrameType.REPLY_ERR, 0, 3, 0, 8, "/回复", "", WireFrame.encodeErrPayload(1, "missing"))
    try {
      ch.writeInbound(Unpooled.wrappedBuffer(ask.take(ask.length - 1)))
      assert(GatewayMetrics.snapshot().bytesRx == before.bytesRx)
      ch.writeInbound(Unpooled.wrappedBuffer(ask.takeRight(1)))
      val received = ch.readInbound[WireFrame.Frame](); received.releaseBuf()
      ch.writeOutbound(reply)
      val sent = ch.readOutbound[ByteBuf]()
      try {
        val after = GatewayMetrics.snapshot()
        assert(after.bytesRx - before.bytesRx == ask.length)
        assert(after.asksRx - before.asksRx == 1)
        assert(after.bytesTx - before.bytesTx == sent.readableBytes())
        assert(after.repliesTx - before.repliesTx == 1)
        assert(after.replyErrs - before.replyErrs == 1)
      } finally sent.release()
    } finally ch.finishAndReleaseAll()
  }

  test("minimum peer limit applies after ACK success and then rejects oversized prefixes") {
    val fields = WireFrame.parseTlv(WireFrame.handshakeBody("peer"))
    val body = fields.flatMap { case (tag, value) =>
      tlv(tag, if (tag == 5) Array[Byte](28, 0, 0, 0) else value)
    }.toArray
    val before = GatewayMetrics.snapshot()
    val ch = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "server"))
    try {
      ch.writeInbound(Unpooled.wrappedBuffer(frame(WireFrame.FrameType.HANDSHAKE, body).encode()))
      assert(ch.isActive)
      val ack = ch.readOutbound[ByteBuf]()
      assert(ack != null)
      try assert(ack.readableBytes() > 32) finally ack.release()
      val after = GatewayMetrics.snapshot()
      assert(after.handshakesOk == before.handshakesOk + 1)
      assert(after.handshakesFailed == before.handshakesFailed)
      assert(after.connections == before.connections + 1)
      ch.writeInbound(Unpooled.wrappedBuffer(Array[Byte](29, 0, 0, 0)))
      assert(!ch.isActive)
      assert(GatewayMetrics.snapshot().connections == before.connections)
      assert(GatewayMetrics.snapshot().handshakesFailed == before.handshakesFailed)
    } finally ch.finishAndReleaseAll()
  }

  test("handshake requires valid identities widths capabilities roles and unique known tags") {
    val fields = WireFrame.parseTlv(WireFrame.handshakeBody("peer"))
    for ((tag, value) <- Seq((1, Array[Byte](0xff.toByte)), (4, Array[Byte](2, 0, 0, 0)),
      (4, Array[Byte](1)), (5, Array[Byte](27, 0, 0, 0)), (6, Array[Byte](4)), (7, Array[Byte](0)))) {
      val body = fields.flatMap { case (id, original) => tlv(id, if (id == tag) value else original) }.toArray
      intercept[IllegalArgumentException] { WireFrame.decodeHandshake(body, acknowledgement = false) }
    }
    for (bad <- Seq(Array[Byte](1), Array[Byte](1, 1), Array[Byte](1, 2, 0, 1)))
      intercept[IllegalArgumentException] { WireFrame.parseTlv(bad) }
    val valid = WireFrame.handshakeBody("peer") ++ tlv(8, "127.0.0.1:123".getBytes("UTF-8")) ++ tlv(99, Array[Byte](42))
    assert(WireFrame.decodeHandshake(valid, acknowledgement = false).nodeId == "peer")
    intercept[IllegalArgumentException] {
      WireFrame.decodeHandshake(WireFrame.handshakeAckBody("peer") ++ tlv(9, "bin".getBytes("UTF-8")), acknowledgement = true)
    }
  }

  test("counter increments saturate at unsigned u64 maximum") {
    val c = new java.util.concurrent.atomic.AtomicLong(Long.MaxValue)
    GatewayMetrics.add(c, 1); assert(c.get() == Long.MinValue)
    c.set(-2L)
    GatewayMetrics.add(c, 3); assert(c.get() == -1L)
    GatewayMetrics.add(c, 1); assert(c.get() == -1L)
  }

  test("listener shutdown waits for accepted sockets and event loops to retire") {
    val ext = new ParrotTransportExtension(null, null, "owned")
    var socket: java.net.Socket = null
    try {
      ext.listen(0)
      socket = new java.net.Socket("127.0.0.1", ext.port)
      socket.setSoTimeout(5000)
      // Observe an ACK to prove the accepted channel is registered with its owner.
      socket.getOutputStream.write(frame(WireFrame.FrameType.HANDSHAKE, WireFrame.handshakeBody("peer")).encode())
      assert(readFrame(socket).frameType == WireFrame.FrameType.HANDSHAKE_ACK)
      ext.shutdown().get(10, java.util.concurrent.TimeUnit.SECONDS)
      assert(socket.getInputStream.read() == -1)
      assert(ext.shutdown().isDone)
      intercept[IllegalArgumentException] { ext.listen(0) }
    } finally {
      if (socket != null) socket.close()
      ext.shutdown().get(10, java.util.concurrent.TimeUnit.SECONDS)
    }
  }

  private def readFrame(socket: java.net.Socket): WireFrame.Frame = {
    val in = new java.io.DataInputStream(socket.getInputStream)
    val size = java.lang.Integer.reverseBytes(in.readInt())
    assert(size >= WireFrame.BODY_FIXED_OVERHEAD && size <= WireFrame.MAX_FRAME_LEN)
    val bytes = new Array[Byte](size + 4)
    java.nio.ByteBuffer.wrap(bytes).order(java.nio.ByteOrder.LITTLE_ENDIAN).putInt(size)
    in.readFully(bytes, 4, size)
    WireFrame.decode(bytes).get._1
  }

  test("register retries as one owned chain and shutdown prevents future dials") {
    val listener = new java.net.ServerSocket(0, 5, java.net.InetAddress.getLoopbackAddress)
    listener.setSoTimeout(5000)
    val ext = new ParrotTransportExtension(null, null, "client")
    var first: java.net.Socket = null
    var second: java.net.Socket = null
    try {
      ext.registerTo("127.0.0.1", listener.getLocalPort)
      first = listener.accept(); first.setSoTimeout(5000)
      assert(readFrame(first).frameType == WireFrame.FrameType.HANDSHAKE)
      first.getOutputStream.write(frame(WireFrame.FrameType.HANDSHAKE_ACK, Array.emptyByteArray).encode())
      assert(first.getInputStream.read() == -1)
      second = listener.accept(); second.setSoTimeout(5000)
      assert(readFrame(second).frameType == WireFrame.FrameType.HANDSHAKE)
      second.getOutputStream.write(frame(WireFrame.FrameType.HANDSHAKE_ACK, WireFrame.handshakeAckBody("hub")).encode())
      // A heartbeat reply proves the valid ACK reached the active client driver.
      second.getOutputStream.write(frame(WireFrame.FrameType.HEARTBEAT, Array.emptyByteArray).encode())
      assert(readFrame(second).frameType == WireFrame.FrameType.HEARTBEAT_ACK)
      ext.shutdown().get(10, java.util.concurrent.TimeUnit.SECONDS)
      assert(second.getInputStream.read() == -1)
      listener.setSoTimeout(1300)
      intercept[java.net.SocketTimeoutException] { val unexpected = listener.accept(); unexpected.close() }
    } finally {
      if (first != null) first.close()
      if (second != null) second.close()
      ext.shutdown().get(10, java.util.concurrent.TimeUnit.SECONDS)
      listener.close()
    }
  }

}
