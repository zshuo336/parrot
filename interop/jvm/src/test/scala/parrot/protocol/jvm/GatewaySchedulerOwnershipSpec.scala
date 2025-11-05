package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.Behaviors
import org.scalatest.funsuite.AnyFunSuite
import java.io.DataInputStream
import java.net.{ServerSocket, Socket}
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.TimeUnit
import scala.concurrent.Await
import scala.concurrent.duration._
import BridgeActor._

object GatewaySchedulerOwnershipSpec {
  private def read(socket: Socket): WireFrame.Frame = {
    val input = new DataInputStream(socket.getInputStream)
    val prefix = new Array[Byte](4)
    input.readFully(prefix)
    val size = ByteBuffer.wrap(prefix).order(ByteOrder.LITTLE_ENDIAN).getInt
    assert(size >= 28 && size <= 16 * 1024 * 1024)
    val body = new Array[Byte](size)
    input.readFully(body)
    WireFrame.decode(prefix ++ body).get._1
  }
  private def send(socket: Socket, kind: Byte, cid: Long, payload: Array[Byte]): Unit = {
    socket.getOutputStream.write(WireFrame.Frame(1, kind, 0, cid, 0, 8, "/user/echo", "bin:echo", payload).encode())
    socket.getOutputStream.flush()
  }

  def exercise(register: Boolean, initializeLegacy: Boolean): Unit = {
      val owner = ActorSystem[Any](Behaviors.empty, "scheduler-owner")
      val unrelated = ActorSystem[Any](Behaviors.empty, "scheduler-unrelated")
      val requests = new java.util.concurrent.LinkedBlockingQueue[Ask]()
      val bridge = owner.systemActorOf(Behaviors.receiveMessage[BridgeMsg] {
        case ask: Ask => requests.add(ask); Behaviors.same
        case _ => Behaviors.same
      }, "bridge")
      val ext = new ParrotTransportExtension(owner, bridge, "owned-gateway")
      var socket: Socket = null
      var listener: ServerSocket = null
      try {
        // Reproduce the historical public bootstrap from another embedding.
        // A retired foreign scheduler must never govern this owner's requests.
        if (initializeLegacy) ParrotServerHandler.initSystem(unrelated)
        if (register) {
          listener = new ServerSocket(0, 4, java.net.InetAddress.getLoopbackAddress)
          listener.setSoTimeout(3000)
          ext.registerTo("127.0.0.1", listener.getLocalPort)
          socket = listener.accept()
          socket.setSoTimeout(3000)
          assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE)
          send(socket, WireFrame.FrameType.HANDSHAKE_ACK, 0, WireFrame.handshakeAckBody("hub"))
          send(socket, WireFrame.FrameType.HEARTBEAT, 0, Array.emptyByteArray)
          assert(read(socket).frameType == WireFrame.FrameType.HEARTBEAT_ACK)
        } else {
          ext.listen(0)
          socket = new Socket("127.0.0.1", ext.port)
          socket.setSoTimeout(3000)
          send(socket, WireFrame.FrameType.HANDSHAKE, 0, WireFrame.handshakeBody("peer"))
          assert(read(socket).frameType == WireFrame.FrameType.HANDSHAKE_ACK)
        }
        for (id <- 1L to 3L) {
          val payload = Array[Byte](42, id.toByte, -1)
          send(socket, WireFrame.FrameType.ASK, id, WireFrame.withReplyToPrefix("/receipt", payload))
          val accepted = requests.poll(3, TimeUnit.SECONDS)
          assert(accepted != null, "request did not reach its owning actor")
          if (id == 1L) {
            // Retiring an unrelated system must leave this pending request usable.
            unrelated.terminate()
            Await.result(unrelated.whenTerminated, 5.seconds)
          }
          accepted.replyTo ! Replied(accepted.cid, accepted.typeKey, accepted.payload)
          val reply = read(socket)
          assert(reply.frameType == WireFrame.FrameType.REPLY && reply.correlationId == id)
          assert(reply.path == "/receipt" && reply.typeKey == "bin:echo")
          assert(reply.payload.sameElements(payload))
        }
      } finally {
        if (socket != null) socket.close()
        if (listener != null) listener.close()
        try ext.shutdown().get(10, TimeUnit.SECONDS)
        finally {
          unrelated.terminate(); owner.terminate()
          Await.result(unrelated.whenTerminated, 5.seconds)
          Await.result(owner.whenTerminated, 5.seconds)
        }
      }
    }
}

class GatewaySchedulerOwnershipSpec extends AnyFunSuite {
  for (register <- Seq(false, true)) {
    val mode = if (register) "registration" else "listener"
    test(s"$mode ASK uses its owner after another ActorSystem retires") {
      GatewaySchedulerOwnershipSpec.exercise(register, initializeLegacy = true)
    }
    test(s"$mode works in a fresh JVM without hidden global bootstrap") {
      val output = java.nio.file.Files.createTempFile("parrot-bootstrap-", ".log")
      val javaBinary = java.nio.file.Paths.get(System.getProperty("java.home"), "bin", "java").toString
      val child = new ProcessBuilder(javaBinary, "-cp", System.getProperty("java.class.path"),
        "parrot.protocol.jvm.GatewaySchedulerBootstrapProbe", mode)
        .redirectErrorStream(true).redirectOutput(output.toFile).start()
      try {
        assert(child.waitFor(20, TimeUnit.SECONDS), s"bootstrap child timed out: $output")
        val report = java.nio.file.Files.readString(output)
        assert(child.exitValue() == 0, s"bootstrap child failed ($output): $report")
      } finally {
        if (child.isAlive) { child.destroyForcibly(); assert(child.waitFor(5, TimeUnit.SECONDS)) }
      }
    }
  }
}
object GatewaySchedulerBootstrapProbe {
  def main(args: Array[String]): Unit =
    GatewaySchedulerOwnershipSpec.exercise(args(0) == "registration", initializeLegacy = false)
}
