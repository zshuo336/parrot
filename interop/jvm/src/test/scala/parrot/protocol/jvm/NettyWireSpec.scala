package parrot.protocol.jvm

import io.netty.buffer.{ByteBuf, Unpooled}
import io.netty.channel.embedded.EmbeddedChannel
import org.scalatest.funsuite.AnyFunSuite
import scala.io.Source
import scala.util.Using
import scala.util.control.NonFatal

/** The same independent corpus exercises both public and production decoders. */
class NettyWireSpec extends AnyFunSuite {
  private def hex(text: String): Array[Byte] = text.grouped(2).map(Integer.parseInt(_, 16).toByte).toArray
  private val corpus = Using.resource(Source.fromFile("../../docs/vectors/wire1-boundaries.tsv")) { source =>
    source.getLines().filterNot(_.startsWith("#")).map(_.split("\t", -1)).toVector
  }

  corpus.foreach { row =>
    val Array(name, expected, encoded) = row
    test(s"array decoder shared boundary $name") {
      val bytes = hex(encoded)
      if (expected == "reject") intercept[IllegalArgumentException] { WireFrame.decode(bytes) }
      else {
        val decoded = WireFrame.decode(bytes)
        assert(decoded.isDefined == (expected == "frame"))
        decoded.foreach { case (frame, consumed) =>
          assert(consumed == bytes.length)
          assert(frame.encode().sameElements(bytes), "valid extension/header bytes must survive roundtrip")
        }
      }
    }

    test(s"production decoder shared boundary $name") {
      val input = Unpooled.wrappedBuffer(hex(encoded))
      val channel = new EmbeddedChannel(new WireDecoder())
      var failure = false
      try {
        try channel.writeInbound(input) catch { case NonFatal(_) => failure = true }
        val frame = channel.readInbound[WireFrame.Frame]()
        if (expected == "reject") {
          try {
            assert(failure || !channel.isActive, "malformed complete header/frame was treated as valid or partial")
            assert(frame == null, "malformed input reached dispatch")
          } finally if (frame != null) frame.releaseBuf()
        } else if (expected == "more") {
          assert(!failure && channel.isActive && frame == null)
        } else {
          assert(!failure && frame != null)
          try assert(frame.encode().sameElements(hex(encoded)))
          finally frame.releaseBuf()
        }
      } finally {
        var left = channel.readInbound[WireFrame.Frame]()
        while (left != null) { left.releaseBuf(); left = channel.readInbound[WireFrame.Frame]() }
        channel.finishAndReleaseAll()
        if (input.capacity() > 0 && input.refCnt() > 0) input.release(input.refCnt())
      }
    }
  }

  test("production server retires the retained inbound payload") {
    val encoded = WireFrame.Frame(1, WireFrame.FrameType.HANDSHAKE, 0, 1, 0, 8, "", "",
      WireFrame.handshakeBody("peer")).encode()
    val input = Unpooled.wrappedBuffer(encoded)
    val channel = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotServerHandler(null, "server"))
    try {
      channel.writeInbound(input)
      val reply = channel.readOutbound[ByteBuf]()
      assert(reply != null)
      reply.release()
      channel.finishAndReleaseAll()
      assert(input.refCnt() == 0, "server discarded Frame without releasing its retained ByteBuf")
    } finally {
      channel.finishAndReleaseAll()
      if (input.capacity() > 0 && input.refCnt() > 0) input.release(input.refCnt())
    }
  }

  test("every split point preserves partial input and emits both pipelined frames in order") {
    val first = hex(corpus.find(_(0) == "unicode-valid").get(2))
    val second = hex(corpus.find(_(0) == "sequence-low32-valid").get(2))
    for (split <- 0 until first.length) {
      val channel = new EmbeddedChannel(new WireDecoder())
      try {
        channel.writeInbound(Unpooled.wrappedBuffer(first.take(split)))
        assert(channel.readInbound[WireFrame.Frame]() == null)
        channel.writeInbound(Unpooled.wrappedBuffer(first.drop(split) ++ second))
        for (expected <- Seq(first, second)) {
          val frame = channel.readInbound[WireFrame.Frame]()
          assert(frame != null)
          try assert(frame.encode().sameElements(expected)) finally frame.releaseBuf()
        }
        assert(channel.readInbound[WireFrame.Frame]() == null)
      } finally channel.finishAndReleaseAll()
    }
  }

  test("shared parser respects a nonzero reader index and a composite buffer") {
    val encoded = hex(corpus.find(_(0) == "unicode-valid").get(2))
    val input = Unpooled.compositeBuffer()
    input.addComponents(true, Unpooled.wrappedBuffer(Array[Byte](99, 98) ++ encoded.take(35)),
      Unpooled.wrappedBuffer(encoded.drop(35)))
    input.skipBytes(2)
    try {
      val (frame, consumed) = WireFrame.decodeBuffer(input).get
      try {
        assert(input.readerIndex() == 2)
        assert(consumed == encoded.length)
        assert(frame.encode().sameElements(encoded))
        assert(frame.encode().sameElements(encoded), "encoding must not consume the retained payload")
      } finally frame.releaseBuf()
    } finally input.release()
  }

  test("invalid length cannot consume the next frame's key or payload") {
    val invalid = hex(corpus.find(_(0) == "empty-valid").get(2))
    invalid(24) = 1 // the empty frame has no room for a path
    val valid = hex(corpus.find(_(0) == "unicode-valid").get(2))
    val channel = new EmbeddedChannel(new WireDecoder())
    try {
      channel.writeInbound(Unpooled.wrappedBuffer(invalid ++ valid))
      assert(!channel.isActive)
      assert(channel.readInbound[WireFrame.Frame]() == null)
    } finally channel.finishAndReleaseAll()
  }

  test("client and server release inbound buffers on acceptance rejection and handler exception") {
    for (client <- Seq(false, true); rejected <- Seq(false, true)) {
      val handler = if (client) new ParrotClientHandler(null, "client") else new ParrotServerHandler(null, "server")
      val channel = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), handler)
      val ft = if (rejected) WireFrame.FrameType.TELL
        else if (client) WireFrame.FrameType.HANDSHAKE_ACK else WireFrame.FrameType.HANDSHAKE
      val payload = if (client) WireFrame.handshakeAckBody("peer") else WireFrame.handshakeBody("peer")
      val input = Unpooled.wrappedBuffer(WireFrame.Frame(1, ft, 0, 1, 0, 8, "", "", payload).encode())
      try {
        channel.writeInbound(input)
        channel.finishAndReleaseAll()
        assert(input.refCnt() == 0)
      } finally {
        channel.finishAndReleaseAll()
        if (input.refCnt() > 0) input.release(input.refCnt())
      }
    }
    val handler = new ParrotServerHandler(null, "server") {
      override protected def handleFrame(ctx: io.netty.channel.ChannelHandlerContext, msg: WireFrame.Frame): Unit =
        throw new IllegalStateException("injected dispatch failure")
    }
    val channel = new EmbeddedChannel(new WireDecoder(), handler)
    val input = Unpooled.wrappedBuffer(hex(corpus.find(_(0) == "unicode-valid").get(2)))
    try {
      channel.writeInbound(input)
      channel.finishAndReleaseAll()
      assert(input.refCnt() == 0)
    } finally {
      channel.finishAndReleaseAll()
      if (input.refCnt() > 0) input.release(input.refCnt())
    }
  }

  test("both encoders reject invalid outbound headers Unicode and lengths and release failed sends") {
    for (kind <- Seq("version", "type", "hop", "flags", "sequence", "unicode", "oversize")) {
      val input = Unpooled.buffer(if (kind == "oversize") WireFrame.MAX_FRAME_LEN else 1)
      input.writeZero(if (kind == "oversize") WireFrame.MAX_FRAME_LEN else 1)
      val frame = new WireFrame.Frame(if (kind == "version") 2 else 1,
        if (kind == "type") 0 else WireFrame.FrameType.TELL,
        if (kind == "flags") 65536 else 0, 1, 0, if (kind == "hop") 0 else 8,
        if (kind == "unicode") "\ud800" else "", "", null, input,
        if (kind == "sequence") 0x100000000L else 0)
      val channel = new EmbeddedChannel(new WireEncoder())
      try {
        intercept[IllegalArgumentException] { frame.encode() }
        assert(input.refCnt() == 1, "pure encode must leave the caller's ownership intact")
        intercept[io.netty.handler.codec.EncoderException] { channel.writeOutbound(frame) }
        assert(input.refCnt() == 0, "failed send must retire the retained payload")
        assert(channel.readOutbound[ByteBuf]() == null)
      } finally {
        frame.releaseBuf()
        channel.finishAndReleaseAll()
      }
    }
  }

  for (failure <- Seq("encode", "peer-limit", "transport", "prepare"); void <- Seq(false, true)) {
    test(s"failed $failure write with void=$void retires its connection and owned payload") {
      var transportWrites = 0
      val transport = new io.netty.channel.ChannelOutboundHandlerAdapter {
        override def write(ctx: io.netty.channel.ChannelHandlerContext, msg: Any,
                           promise: io.netty.channel.ChannelPromise): Unit = {
          transportWrites += 1
          io.netty.util.ReferenceCountUtil.release(msg)
          promise.setFailure(new java.io.IOException("injected transport write failure"))
        }
      }
      val observer = new io.netty.channel.ChannelInboundHandlerAdapter {
        override def exceptionCaught(ctx: io.netty.channel.ChannelHandlerContext, cause: Throwable): Unit = ()
      }
      val channel = new EmbeddedChannel(transport, new WireEncoder(), observer)
      channel.attr(WireFrame.PeerFrameLimit).set(java.lang.Long.valueOf(if (failure == "peer-limit") 28 else 1024))
      val input = Unpooled.buffer().writeByte(42)
      val frame = new WireFrame.Frame(1, WireFrame.FrameType.REPLY, 0, 1, 0, 8,
        if (failure == "prepare") null else "", if (failure == "encode") "\ud800" else "", input)
      val promise = if (void) channel.voidPromise() else channel.newPromise()
      try {
        channel.pipeline().writeAndFlush(frame, promise)
        channel.runPendingTasks()
        assert(input.refCnt() == 0, "owned payload must be released exactly once")
        assert(!channel.isActive, "failed write must retire the connection")
        assert(transportWrites == (if (failure == "transport") 1 else 0))
        if (!void) assert(promise.isDone && !promise.isSuccess)
      } finally { frame.releaseBuf(); channel.finishAndReleaseAll() }
    }
  }

  test("outbound write failure retires the payload") {
    val input = Unpooled.buffer().writeByte(42)
    val frame = new WireFrame.Frame(1, WireFrame.FrameType.TELL, 0, 1, 0, 8, "", "", input)
    val output = Unpooled.buffer(1, 1)
    try {
      intercept[IndexOutOfBoundsException] { new WireEncoder().encode(null, frame, output) }
      assert(input.refCnt() == 0)
    } finally { frame.releaseBuf(); output.release() }
  }

  test("reply-to encoder rejects malformed Unicode without rewriting the return identity") {
    for (value <- Seq("\ud800", "\udc00", "/reply/\ud800x")) {
      intercept[IllegalArgumentException] { WireFrame.withReplyToPrefix(value, Array[Byte](42)) }
    }
    for (value <- Seq("", "/目录/🙂", "\ufeff/reply", "a\u0000b")) {
      val encoded = WireFrame.withReplyToPrefix(value, Array[Byte](0, -1, 42))
      val (actual, body) = WireFrame.splitReplyTo(encoded).get
      assert(actual == value)
      assert(body.sameElements(Array[Byte](0, -1, 42)))
    }
  }

  test("reply-to prefix rejects unsigned overflow truncation and malformed UTF-8") {
    for (prefix <- Seq("ffffffff", "00000080", "01000000", "01000000ff"))
      assert(WireFrame.splitReplyTo(hex(prefix)).isEmpty)
    val value = WireFrame.withReplyToPrefix("/目录/🙂", Array[Byte](0, -1, 42))
    val (path, body) = WireFrame.splitReplyTo(value).get
    assert(path == "/目录/🙂")
    assert(body.sameElements(Array[Byte](0, -1, 42)))
  }

}
