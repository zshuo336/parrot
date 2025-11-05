package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.Behaviors
import org.scalatest.funsuite.AnyFunSuite
import java.io.DataInputStream
import java.net.{ServerSocket, Socket}
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.{CountDownLatch, LinkedBlockingQueue, TimeUnit}
import scala.concurrent.Await
import scala.concurrent.duration._
import WireFrame.FrameType._

/** A management callback owns one receipt for the original wire request. */
class AdminReceiptOwnershipSpec extends AnyFunSuite {
  private class Session(registering: Boolean, adminEnabled: Boolean) {
    val commands = new LinkedBlockingQueue[AdminPort.CmdIn]()
    val system = ActorSystem[Any](Behaviors.empty, "admin-receipts")
    val admin = system.systemActorOf(Behaviors.receiveMessage[AdminPort.Msg] {
      case c: AdminPort.CmdIn => commands.add(c); Behaviors.same
      case _ => Behaviors.same
    }, "admin")
    val bridge = system.systemActorOf(Behaviors.ignore[BridgeActor.BridgeMsg], "bridge")
    val transport = new ParrotTransportExtension(system, bridge, "admin-receipts", if (adminEnabled) Some(admin) else None)
    val hub = if (registering) new ServerSocket(0) else null
    var socket: Socket = null
    var input: DataInputStream = null
    def start(): Unit = {
      if (registering) {
        hub.setSoTimeout(4000)
        transport.registerTo("127.0.0.1", hub.getLocalPort)
      } else transport.listen(0)
      connect()
    }
    def connect(): Unit = {
      if (socket != null) socket.close()
      socket = if (registering) hub.accept() else new Socket("127.0.0.1", transport.port)
      socket.setSoTimeout(4000)
      input = new DataInputStream(socket.getInputStream)
      if (registering) {
        assert(read().frameType == HANDSHAKE)
        send(HANDSHAKE_ACK, 0, "", WireFrame.handshakeAckBody("hub"))
        barrier()
      } else {
        send(HANDSHAKE, 0, "", WireFrame.handshakeBody("peer"))
        assert(read().frameType == HANDSHAKE_ACK)
      }
    }
    def send(kind: Byte, cid: Long, path: String, payload: Array[Byte]): Unit = {
      socket.getOutputStream.write(WireFrame.Frame(1, kind, 0, cid, 0, 8, path, "", payload).encode())
      socket.getOutputStream.flush()
    }
    def request(cid: Long, path: String = "/receipt"): AdminPort.CmdIn = {
      send(SYSTEM_EVENT, cid, path, AdminV2Codec.encodeCommand(AdminV2Codec.AdminCommandV2.MetricsReport(cid)))
      val command = commands.poll(3, TimeUnit.SECONDS)
      assert(command != null, "management admission barrier")
      assert(command.cid == cid && command.replyTo == path)
      command
    }
    def read(): WireFrame.Frame = {
      val prefix = new Array[Byte](4); input.readFully(prefix)
      val size = ByteBuffer.wrap(prefix).order(ByteOrder.LITTLE_ENDIAN).getInt
      assert(size >= 28 && size <= WireFrame.MAX_FRAME_LEN,
        s"invalid prefix ${prefix.map(b => f"${b & 255}%02x").mkString}")
      val bytes = new Array[Byte](size); input.readFully(bytes)
      WireFrame.decode(prefix ++ bytes).get._1
    }
    def barrier(): Unit = {
      send(HEARTBEAT, 0, "", Array.emptyByteArray)
      assert(read().frameType == HEARTBEAT_ACK, "unexpected extra management receipt before heartbeat")
    }
    def close(): Unit = {
      try {
        if (socket != null) socket.close()
        if (hub != null) hub.close()
        transport.shutdown().get(10, TimeUnit.SECONDS)
      } finally { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
    }
  }
  private def session(registering: Boolean, adminEnabled: Boolean = true)(run: Session => Unit): Unit = {
    val s = new Session(registering, adminEnabled)
    try { s.start(); run(s) } finally s.close()
  }
  private def stopped(cid: Long): Array[Byte] = AdminV2Codec.encodeReply(AdminV2Codec.AdminReplyV2.Stopped(cid))
  private def receipt(s: Session, cid: Long, path: String, body: Array[Byte]): Unit = {
    val frame = s.read()
    assert(frame.frameType == SYSTEM_EVENT && frame.correlationId == cid && frame.path == path)
    assert(frame.payload.sameElements(body))
  }

  for (registering <- Seq(false, true)) {
    test(s"disabled management returns its failure to the original request path, registering=$registering") {
      session(registering, adminEnabled = false) { s =>
        val body = AdminV2Codec.encodeReply(AdminV2Codec.AdminReplyV2.Failed(
          17, AdminV2Executor.ErrDialectMismatch, "admin port disabled"))
        s.send(SYSTEM_EVENT, 17, "/disabled/节点",
          AdminV2Codec.encodeCommand(AdminV2Codec.AdminCommandV2.MetricsReport(17)))
        receipt(s, 17, "/disabled/节点", body)
        s.barrier()
      }
    }
    test(s"management receipt identity belongs to its wire request, registering=$registering") {
      session(registering) { s =>
        val request = s.request(7, "/receipt/节点")
        val body = stopped(7)
        request.sink.send(999, "/another-request", body)
        receipt(s, 7, request.replyTo, body)
        s.barrier()
      }
    }
    test(s"one management command cannot publish two terminal receipts, registering=$registering") {
      session(registering) { s =>
        val request = s.request(8)
        val body = stopped(8)
        request.sink.send(8, request.replyTo, body)
        request.sink.send(8, request.replyTo, AdminV2Codec.encodeReply(
          AdminV2Codec.AdminReplyV2.Failed(8, 1, "late worker failure")))
        receipt(s, 8, request.replyTo, body)
        s.barrier()
      }
    }
    test(s"retired sink cannot complete a newer same-id request, registering=$registering") {
      session(registering) { s =>
        val old = s.request(9)
        old.sink.send(9, old.replyTo, stopped(9))
        receipt(s, 9, old.replyTo, stopped(9))
        val next = s.request(9, "/new-receipt")
        old.sink.send(9, next.replyTo, stopped(9))
        val body = AdminV2Codec.encodeReply(AdminV2Codec.AdminReplyV2.Failed(9, 1, "new request result"))
        next.sink.send(9, next.replyTo, body)
        receipt(s, 9, next.replyTo, body)
        s.barrier()
      }
    }
    test(s"concurrent completion publishes exactly one whole receipt, registering=$registering") {
      session(registering) { s =>
        val request = s.request(10)
        val start = new CountDownLatch(1)
        val done = new CountDownLatch(16)
        val failures = new LinkedBlockingQueue[Throwable]()
        val bodies = (0 until 16).map(i => AdminV2Codec.encodeReply(
          AdminV2Codec.AdminReplyV2.Failed(10, 1, s"result-$i")))
        val workers = bodies.map { body =>
          val worker = new Thread(() => {
            try { assert(start.await(3, TimeUnit.SECONDS)); request.sink.send(10, request.replyTo, body) }
            catch { case error: Throwable => failures.add(error) }
            finally done.countDown()
          }, "admin-receipt-completion")
          worker.start(); worker
        }
        try {
          start.countDown()
          assert(done.await(5, TimeUnit.SECONDS), "completion workers did not finish")
          assert(failures.isEmpty)
          val actual = s.read()
          assert(actual.correlationId == 10 && actual.path == request.replyTo)
          assert(bodies.exists(_.sameElements(actual.payload)))
          s.barrier()
        } finally {
          start.countDown()
          workers.foreach(_.join(5000))
          assert(workers.forall(!_.isAlive))
        }
      }
    }
    test(s"independent requests retain independent receipts and errors, registering=$registering") {
      session(registering) { s =>
        val a = s.request(11, "")
        val b = s.request(12, "/other")
        val error = AdminV2Codec.encodeReply(AdminV2Codec.AdminReplyV2.Failed(12, 1, "expected error"))
        b.sink.send(12, b.replyTo, error)
        a.sink.send(11, a.replyTo, stopped(11))
        receipt(s, 12, b.replyTo, error)
        receipt(s, 11, a.replyTo, stopped(11))
        s.barrier()
      }
    }
    test(s"an uncompleted old connection sink cannot reply on a replacement connection, registering=$registering") {
      session(registering) { s =>
        val old = s.request(13)
        s.socket.close()
        s.connect()
        val next = s.request(13)
        old.sink.send(13, next.replyTo, stopped(13))
        val body = AdminV2Codec.encodeReply(AdminV2Codec.AdminReplyV2.Failed(13, 1, "replacement result"))
        next.sink.send(13, next.replyTo, body)
        receipt(s, 13, next.replyTo, body)
        s.barrier()
      }
    }
    test(s"an invalid first receipt closes only its connection and cannot be replaced by a later success, registering=$registering") {
      session(registering) { s =>
        val request = s.request(14)
        request.sink.send(14, request.replyTo, null)
        request.sink.send(14, request.replyTo, stopped(14))
        assert(s.input.read() == -1, "invalid first response must retire the original connection")
        s.connect()
        val next = s.request(14)
        next.sink.send(14, next.replyTo, stopped(14))
        receipt(s, 14, next.replyTo, stopped(14))
        s.barrier()
      }
    }
  }
}
