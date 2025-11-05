package parrot.protocol.jvm

import java.net.{InetAddress, ServerSocket, Socket}
import java.util.concurrent.TimeUnit
import java.nio.{ByteBuffer, ByteOrder}
import org.scalatest.funsuite.AnyFunSuite

class NegotiatedLimitSpec extends AnyFunSuite {
  private def tlv(tag: Int, value: Array[Byte]) = Array(tag.toByte, value.length.toByte, (value.length >> 8).toByte) ++ value
  private def prefix(value: Int) = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(value).array()
  private def hello(limit: Int, ack: Boolean) = tlv(1, Array[Byte](112)) ++ tlv(4, Array[Byte](1, 0, 0, 0)) ++
    tlv(5, prefix(limit)) ++ tlv(6, Array[Byte](0)) ++ (if (ack) tlv(9, Array[Byte](98, 105, 110)) else Array.emptyByteArray)
  private def frame(ft: Byte, body: Array[Byte]) = WireFrame.Frame(1, ft, 0, 1, 0, 8, "", "", body).encode()
  private def read(socket: Socket): WireFrame.Frame = {
    val in = new java.io.DataInputStream(socket.getInputStream)
    val length = java.lang.Integer.reverseBytes(in.readInt())
    assert(length >= 28 && length <= WireFrame.MAX_FRAME_LEN, s"unexpected prefix $length")
    val data = prefix(length) ++ new Array[Byte](length)
    in.readFully(data, 4, length)
    WireFrame.decode(data).get._1
  }

  for (client <- Seq(false, true); limit <- Seq(28, 256); pipelined <- Seq(false, true)) {
    test(s"TCP client=$client limit=$limit pipelined=$pipelined applies bounds after handshake") {
      val ext = new ParrotTransportExtension(null, null, "limit")
      val listener = if (client) new ServerSocket(0, 5, InetAddress.getLoopbackAddress) else null
      var socket: Socket = null
      try {
        if (client) {
          listener.setSoTimeout(5000)
          ext.registerTo("127.0.0.1", listener.getLocalPort)
          socket = listener.accept()
        } else {
          ext.listen(0)
          socket = new Socket("127.0.0.1", ext.port)
        }
        socket.setSoTimeout(2000)
        if (client) assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE)
        val ft = if (client) WireFrame.FrameType.HANDSHAKE_ACK else WireFrame.FrameType.HANDSHAKE
        val tail = if (pipelined) prefix(limit + 1) else frame(WireFrame.FrameType.HEARTBEAT, Array.emptyByteArray)
        socket.getOutputStream.write(frame(ft, hello(limit, client)) ++ tail)
        if (!client) assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE_ACK)
        if (!pipelined) {
          assert(read(socket).frameType == WireFrame.FrameType.HEARTBEAT_ACK)
          // Send just the prefix: decoder must reject before buffering the body.
          socket.getOutputStream.write(prefix(limit + 1))
        }
        assert(socket.getInputStream.read() == -1)
      } finally {
        if (socket != null) socket.close()
        ext.shutdown().get(10, TimeUnit.SECONDS)
        if (listener != null) listener.close()
      }
    }
  }

  test("established decoder accepts the exact limit and rejects the next byte at a nonzero reader index") {
    val channel = new io.netty.channel.embedded.EmbeddedChannel(new WireDecoder())
    channel.attr(WireFrame.PeerFrameLimit).set(java.lang.Long.valueOf(256))
    val encoded = frame(WireFrame.FrameType.TELL, Array.fill[Byte](256 - 28)(42))
    val input = io.netty.buffer.Unpooled.buffer().writeInt(123).writeBytes(encoded)
    input.skipBytes(4)
    try {
      channel.writeInbound(input)
      val decoded = channel.readInbound[WireFrame.Frame]()
      assert(decoded != null)
      try assert(decoded.payload.sameElements(Array.fill[Byte](228)(42)))
      finally decoded.releaseBuf()
      channel.writeInbound(io.netty.buffer.Unpooled.wrappedBuffer(prefix(257)))
      assert(!channel.isActive)
      assert(channel.readInbound[WireFrame.Frame]() == null)
    } finally channel.finishAndReleaseAll()
  }
}
