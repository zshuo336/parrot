package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import java.io.DataInputStream
import java.net.Socket
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.{Callable, Executors, TimeUnit}

/** Independent complete-byte receipts from the production TCP pipeline. */
class HandshakeWireIntegritySpec extends AnyFunSuite {
  private def le(value: Int, width: Int): Array[Byte] =
    Array.tabulate(width)(i => (value >>> (8 * i)).toByte)
  private def tlv(tag: Int, value: Array[Byte]): Array[Byte] =
    Array(tag.toByte) ++ le(value.length, 2) ++ value
  private def fields(node: String, capabilities: Int = 1): Array[Byte] =
    tlv(1, node.getBytes("UTF-8")) ++ tlv(4, le(capabilities, 4)) ++
      tlv(5, le(16 * 1024 * 1024, 4)) ++ tlv(6, Array[Byte](0)) ++ tlv(7, Array[Byte](8))
  // Neither input nor expected receipts use production Frame/handshake encoders.
  private def frame(kind: Int, body: Array[Byte], cid: Long = 0): Array[Byte] = {
    val header = ByteBuffer.allocate(32).order(ByteOrder.LITTLE_ENDIAN)
    header.putInt(28 + body.length).put(1.toByte).put(kind.toByte).putShort(0.toShort)
      .putLong(cid).put(0.toByte).put(8.toByte).putInt(0).putShort(0.toShort)
      .putInt(0).putInt(0)
    header.array() ++ body
  }
  private def hex(bytes: Array[Byte]): String = bytes.map(b => f"${b & 255}%02x").mkString
  private def receipt(socket: Socket, expected: Array[Byte], context: String): Unit = {
    // Never allocate or advance a parser based on the suspect received prefix.
    val input = new DataInputStream(socket.getInputStream)
    val actual = new Array[Byte](expected.length)
    try input.readFully(actual)
    catch { case error: java.io.IOException =>
      fail(s"$context incomplete receipt ${hex(actual)}", error)
    }
    assert(actual.sameElements(expected), s"$context expected=${hex(expected)} actual=${hex(actual)}")
  }

  test("production TCP ACK and heartbeat bytes remain exact across concurrent connection turnover") {
    val transport = new ParrotTransportExtension(null, null, "wire-integrity")
    val workers = Executors.newFixedThreadPool(8)
    try {
      transport.listen(0)
      val jobs = (0 until 8).map { worker => workers.submit(new Callable[Unit] {
        override def call(): Unit = for (round <- 0 until 64) {
          val socket = new Socket("127.0.0.1", transport.port)
          try {
            socket.setSoTimeout(3000)
            val label = s"worker=$worker round=$round local=${socket.getLocalSocketAddress} remote=${socket.getRemoteSocketAddress}"
            val node = s"peer-$worker-$round-节点"
            val unknown = if (round % 4 == 0) tlv(99, Array.fill[Byte](65535)((round + worker).toByte)) else Array.emptyByteArray
            val hello = frame(1, fields(node) ++ unknown)
            val cut = 1 + round % 31
            socket.getOutputStream.write(hello, 0, cut)
            socket.getOutputStream.write(hello, cut, hello.length - cut)
            socket.getOutputStream.flush()
            receipt(socket, frame(2, fields("wire-integrity") ++ tlv(9, "bin".getBytes("UTF-8"))), label + " ACK")
            for (n <- 0 until 4) {
              socket.getOutputStream.write(frame(3, Array.emptyByteArray))
              socket.getOutputStream.flush()
              receipt(socket, frame(4, Array.emptyByteArray), label + s" heartbeat=$n")
            }
          } finally socket.close()
        }
      }) }
      jobs.foreach(_.get(60, TimeUnit.SECONDS))
    } finally {
      try {
        workers.shutdownNow()
        assert(workers.awaitTermination(10, TimeUnit.SECONDS), "socket workers did not retire")
      } finally transport.shutdown().get(10, TimeUnit.SECONDS)
    }
  }

  test("ACK bytes survive repeated ActorSystem and event-loop retirement with the original admin capability") {
    import akka.actor.typed.ActorSystem
    import akka.actor.typed.scaladsl.Behaviors
    import scala.concurrent.Await
    import scala.concurrent.duration._
    for (round <- 0 until 64) {
      val owner = ActorSystem[Any](Behaviors.empty, "ack-turnover")
      val bridge = owner.systemActorOf(Behaviors.ignore[BridgeActor.BridgeMsg], "bridge")
      val admin = owner.systemActorOf(Behaviors.ignore[AdminPort.Msg], "admin")
      val transport = new ParrotTransportExtension(owner, bridge, "envelope", Some(admin))
      var socket: Socket = null
      try {
        transport.listen(0)
        socket = new Socket("127.0.0.1", transport.port)
        socket.setSoTimeout(3000)
        socket.getOutputStream.write(frame(1, fields("envelope-test")))
        socket.getOutputStream.flush()
        receipt(socket, frame(2, fields("envelope", 1 | (1 << 10)) ++ tlv(9, "bin".getBytes("UTF-8"))),
          s"ActorSystem round=$round local=${socket.getLocalSocketAddress} remote=${socket.getRemoteSocketAddress} boundPort=${transport.port} ACK")
        // Match the original failing envelope case after validating its ACK.
        val unsupported = frame(32, Array[Byte](3, 2, 1, 0))
        unsupported(6) = 32
        socket.getOutputStream.write(unsupported)
        socket.getOutputStream.flush()
        assert(socket.getInputStream.read() == -1, "unsupported flag must close the connection")
      } finally {
        if (socket != null) socket.close()
        try transport.shutdown().get(10, TimeUnit.SECONDS)
        finally { owner.terminate(); Await.result(owner.whenTerminated, 5.seconds) }
      }
    }
  }
}
