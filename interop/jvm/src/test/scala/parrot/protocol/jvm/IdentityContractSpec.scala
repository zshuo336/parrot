package parrot.protocol.jvm

import java.net.{InetAddress, ServerSocket, Socket}
import java.util.concurrent.TimeUnit
import org.scalatest.funsuite.AnyFunSuite

class IdentityContractSpec extends AnyFunSuite {
  private def tlv(tag: Int, value: Array[Byte]) = Array(tag.toByte, value.length.toByte, (value.length >> 8).toByte) ++ value
  // Independent fields: do not use the production handshake encoder for malformed peers.
  private def hello(node: String): Array[Byte] = tlv(1, node.getBytes("UTF-8")) ++
    tlv(4, Array[Byte](1, 0, 0, 0)) ++ tlv(5, Array[Byte](0, 0, 16, 0)) ++ tlv(6, Array[Byte](0))
  private def frame(ft: Byte, body: Array[Byte]) = WireFrame.Frame(1, ft, 0, 1, 0, 8, "", "", body).encode()
  private def read(socket: Socket): WireFrame.Frame = {
    val in = new java.io.DataInputStream(socket.getInputStream)
    val length = java.lang.Integer.reverseBytes(in.readInt())
    assert(length >= 28 && length <= WireFrame.MAX_FRAME_LEN)
    val bytes = new Array[Byte](length + 4)
    java.nio.ByteBuffer.wrap(bytes).order(java.nio.ByteOrder.LITTLE_ENDIAN).putInt(length)
    in.readFully(bytes, 4, length)
    WireFrame.decode(bytes).get._1
  }

  test("local HELLO and ACK reject NUL identity and ACK rejects unshared bin") {
    for (node <- Seq("", "\u0000", "left\u0000right", "界" * 21846)) {
      intercept[IllegalArgumentException] { WireFrame.handshakeBody(node) }
      intercept[IllegalArgumentException] { WireFrame.handshakeAckBody(node) }
    }
    for (caps <- Seq(0, 2)) intercept[IllegalArgumentException] { WireFrame.handshakeAckBody("peer", caps) }
  }

  test("remote HELLO and ACK reject embedded NUL with independent TLVs") {
    for (node <- Seq("\u0000", "left\u0000right"); ack <- Seq(false, true)) {
      intercept[IllegalArgumentException] { WireFrame.decodeHandshake(hello(node), ack) }
    }
  }

  test("exact UTF8 identity limit and unknown tags preserve compatibility") {
    val node = "界" * 21845
    for (ack <- Seq(false, true)) {
      val body = if (ack) WireFrame.handshakeAckBody(node) else WireFrame.handshakeBody(node)
      assert(body(1) == -1 && body(2) == -1)
      assert(WireFrame.decodeHandshake(body ++ tlv(99, Array[Byte](-1)) ++ tlv(99, Array[Byte](0)), ack).nodeId == node)
    }
  }

  test("TCP server refuses NUL HELLO and remains available to a valid peer") {
    val ext = new ParrotTransportExtension(null, null, "server")
    try {
      ext.listen(0)
      for (node <- Seq("left\u0000right", "valid界")) {
        val socket = new Socket("127.0.0.1", ext.port)
        socket.setSoTimeout(5000)
        try {
          socket.getOutputStream.write(frame(WireFrame.FrameType.HANDSHAKE, hello(node)))
          socket.getOutputStream.write(frame(WireFrame.FrameType.HEARTBEAT, Array.emptyByteArray))
          if (node.contains('\u0000')) assert(socket.getInputStream.read() == -1)
          else {
            assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE_ACK)
            assert(read(socket).frameType == WireFrame.FrameType.HEARTBEAT_ACK)
          }
        } finally socket.close()
      }
    } finally ext.shutdown().get(10, TimeUnit.SECONDS)
  }

  test("TCP registration refuses NUL ACK then retries a valid peer") {
    val listener = new ServerSocket(0, 5, InetAddress.getLoopbackAddress)
    listener.setSoTimeout(5000)
    val ext = new ParrotTransportExtension(null, null, "client")
    try {
      ext.registerTo("127.0.0.1", listener.getLocalPort)
      for (node <- Seq("left\u0000right", "valid界")) {
        val socket = listener.accept(); socket.setSoTimeout(5000)
        try {
          assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE)
          socket.getOutputStream.write(frame(WireFrame.FrameType.HANDSHAKE_ACK, hello(node)))
          socket.getOutputStream.write(frame(WireFrame.FrameType.HEARTBEAT, Array.emptyByteArray))
          if (node.contains('\u0000')) assert(socket.getInputStream.read() == -1)
          else assert(read(socket).frameType == WireFrame.FrameType.HEARTBEAT_ACK)
        } finally socket.close()
      }
    } finally { ext.shutdown().get(10, TimeUnit.SECONDS); listener.close() }
  }

  private def extensionFlood: Array[Byte] = {
    val field = tlv(255, Array[Byte](42))
    val out = new Array[Byte](field.length * 32768)
    for (n <- 0 until 32768) System.arraycopy(field, 0, out, n * field.length, field.length)
    out
  }

  test("TCP server accepts extension floods and rejects malformed pipelined handshakes") {
    val ext = new ParrotTransportExtension(null, null, "server")
    val extensions = extensionFlood
    try {
      ext.listen(0)
      for (tail <- Seq(Array[Byte](-1), tlv(1, Array[Byte](42)), Array.emptyByteArray)) {
        val socket = new Socket("127.0.0.1", ext.port)
        socket.setSoTimeout(5000)
        try {
          val packet = frame(WireFrame.FrameType.HANDSHAKE, hello("peer") ++ extensions ++ tail) ++
            frame(WireFrame.FrameType.HEARTBEAT, Array.emptyByteArray)
          socket.getOutputStream.write(packet)
          if (tail.nonEmpty) assert(socket.getInputStream.read() == -1)
          else {
            assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE_ACK)
            assert(read(socket).frameType == WireFrame.FrameType.HEARTBEAT_ACK)
          }
        } finally socket.close()
      }
    } finally ext.shutdown().get(10, TimeUnit.SECONDS)
  }

  test("TCP registration rejects bad extension tails then admits a valid extended ACK") {
    val listener = new ServerSocket(0, 5, InetAddress.getLoopbackAddress)
    listener.setSoTimeout(5000)
    val ext = new ParrotTransportExtension(null, null, "client")
    val extensions = extensionFlood
    try {
      ext.registerTo("127.0.0.1", listener.getLocalPort)
      for (tail <- Seq(Array[Byte](-1), tlv(1, Array[Byte](42)), Array.emptyByteArray)) {
        val socket = listener.accept(); socket.setSoTimeout(5000)
        try {
          assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE)
          socket.getOutputStream.write(frame(WireFrame.FrameType.HANDSHAKE_ACK, hello("peer") ++ extensions ++ tail) ++
            frame(WireFrame.FrameType.HEARTBEAT, Array.emptyByteArray))
          if (tail.nonEmpty) assert(socket.getInputStream.read() == -1)
          else assert(read(socket).frameType == WireFrame.FrameType.HEARTBEAT_ACK)
        } finally socket.close()
      }
    } finally { ext.shutdown().get(10, TimeUnit.SECONDS); listener.close() }
  }
}
