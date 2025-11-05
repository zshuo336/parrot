package parrot.protocol.jvm

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.Behaviors
import org.scalatest.funsuite.AnyFunSuite
import java.net.Socket
import java.io.DataInputStream
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.atomic.AtomicInteger
import scala.concurrent.Await
import scala.concurrent.duration._
import WireFrame.FrameType._
import BridgeActor._

/** Actual Netty TCP ingress; rejected envelopes must never reach the actor/admin spies. */
class GatewayEnvelopeSpec extends AnyFunSuite {
  def session(run: Session => Unit): Unit = {
    val s = new Session
    try { s.start(); run(s) } finally s.close()
  }
  for (flag <- Vector(1, 4, 16, 32, 64, 32768); ft <- Vector(ASK, TELL, SYSTEM_EVENT)) {
    test(s"unsupported flag $flag on frame $ft cannot execute business or management") {
      session { s =>
        s.handshake()
        val payload = if (ft == ASK) Array[Byte](0, 0, 0, 0) ++ "mutate".getBytes("UTF-8") else Array[Byte](3, 2, 1, 0)
        s.send(ft, 2, "bin:mutate", payload, flag)
        assert(s.read().isEmpty, "unsupported envelope was not rejected")
        assert(s.effects.get() == 0)
      }
    }
  }
  test("unsupported handshake flags cannot admit a connection") {
    session { s =>
      s.send(HANDSHAKE, 1, "", WireFrame.handshakeBody("bad-handshake"), 1)
      assert(s.read().isEmpty)
      assert(s.effects.get() == 0)
    }
  }
  for ((name, bytes) <- Vector(
    "empty" -> Array.emptyByteArray, "short" -> Array[Byte](0),
    "unsigned-overflow" -> Array.fill[Byte](4)(-1), "truncated" -> Array[Byte](5, 0, 0, 0, 97, 98, 99),
    "invalid-utf8" -> Array[Byte](1, 0, 0, 0, -1))) {
    test(s"$name ASK prefix errors without effects and permits same-connection recovery") {
      session { s =>
        s.handshake(); s.send(ASK, 8, "bin:mutate", bytes)
        val reply = s.read().get
        assert(reply.frameType == REPLY_ERR && reply.correlationId == 8 && reply.path == "")
        assert(WireFrame.decodeErrPayload(reply.payload)._1 == WireFrame.ErrCode.ProtocolViolation)
        assert(s.effects.get() == 0)
        s.send(ASK, 9, "bin:good", Array[Byte](3, 0, 0, 0, 47, 114, 101, 111, 107))
        val good = s.read().get
        assert(good.frameType == REPLY && good.correlationId == 9 && good.path == "/re")
        assert(new String(good.payload, "UTF-8") == "ok")
        assert(s.effects.get() == 1)
      }
    }
  }
  test("legal reply paths and metadata flags preserve success and business errors") {
    session { s =>
      s.handshake()
      for (path <- Vector("", "/reply", "/回复/😀"); key <- Vector("bin:echo", "bin:fail")) {
        val encoded = path.getBytes("UTF-8")
        val payload = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(encoded.length).array() ++ encoded ++ Array[Byte](1, 2, 3)
        s.send(ASK, 5, key, payload, 2 | 8)
        val reply = s.read().get
        assert(reply.frameType == (if (key == "bin:fail") REPLY_ERR else REPLY))
        assert(reply.correlationId == 5 && reply.path == path)
        if (key == "bin:echo") assert(reply.payload.sameElements(Array[Byte](1, 2, 3)))
      }
      assert(s.effects.get() == 6)
    }
  }
  for (key <- Vector("bin:oversized-reply", "bin:bad-reply-key", "bin:bad-error-code", "bin:bad-error-detail")) {
    test(s"$key retires the connection instead of stranding the caller after execution") {
      session { s =>
        s.handshake(128)
        s.send(ASK, 15, key, Array[Byte](0, 0, 0, 0))
        assert(s.invoked.await(3, java.util.concurrent.TimeUnit.SECONDS), "business execution barrier")
        assert(s.effects.get() == 1, "the actual business request must have executed")
        assert(s.read().isEmpty, "undeliverable response must close its connection")
        // The same listener remains usable after retiring only the failed connection.
        s.reconnect(); s.handshake(128)
        s.send(ASK, 16, "bin:echo", Array[Byte](0, 0, 0, 0, 42))
        val next = s.read().get
        assert(next.frameType == REPLY && next.correlationId == 16)
        assert(next.payload.sameElements(Array[Byte](42)))
        assert(s.effects.get() == 2)
      }
    }
  }

  test("ordinary TELL does not parse an ASK prefix") {
    session { s =>
      s.handshake(); s.send(TELL, 0, "bin:mutate", "raw".getBytes("UTF-8"), 2 | 8)
      s.send(ASK, 9, "bin:echo", Array[Byte](0, 0, 0, 0))
      assert(s.read().get.frameType == REPLY)
      assert(s.effects.get() == 2)
      assert(s.lastTell == "raw")
    }
  }
  test("durable envelope cannot execute without durable support") {
    session { s =>
      s.handshake(); s.send(TELL, 3, "parrot:durable#v1", Array[Byte](1))
      assert(s.read().isEmpty)
      assert(s.effects.get() == 0)
    }
  }

  test("registration rejects unsupported ACK flags before becoming ready") {
    import io.netty.buffer.{ByteBuf, Unpooled}
    import io.netty.channel.embedded.EmbeddedChannel
    val channel = new EmbeddedChannel(new WireDecoder(), new WireEncoder(), new ParrotClientHandler(null, "client"))
    try {
      val hello = channel.readOutbound[ByteBuf](); hello.release()
      val ack = WireFrame.Frame(1, HANDSHAKE_ACK, 4, 1, 0, 8, "", "", WireFrame.handshakeAckBody("hub"))
      channel.writeInbound(Unpooled.wrappedBuffer(ack.encode()))
      assert(!channel.isActive)
      assert(channel.attr(WireFrame.HandshakeComplete).get() != java.lang.Boolean.TRUE)
    } finally channel.finishAndReleaseAll()
  }

  class Session {
    val effects = new AtomicInteger
    val invoked = new java.util.concurrent.CountDownLatch(1)
    @volatile var lastTell = ""
    val system = ActorSystem[Any](Behaviors.empty, "gateway-envelope")
    val target = system.systemActorOf(Behaviors.receiveMessage[Any] {
      case BridgeAsk(key, bytes, reply) =>
        effects.incrementAndGet()
        invoked.countDown()
        reply ! (key match {
          case "bin:fail" => BridgeReplyErr(WireFrame.ErrCode.CodecError, "business rejected")
          case "bin:oversized-reply" => BridgeReplyOk(key, new Array[Byte](1024))
          case "bin:bad-reply-key" => BridgeReplyOk("\ud800", bytes)
          case "bin:bad-error-code" => BridgeReplyErr(14, "unknown code")
          case "bin:bad-error-detail" => BridgeReplyErr(WireFrame.ErrCode.CodecError, "\ud800")
          case _ => BridgeReplyOk(key, bytes)
        })
        Behaviors.same
      case BridgeTell(_, bytes) => effects.incrementAndGet(); lastTell = new String(bytes, "UTF-8"); Behaviors.same
      case _ => Behaviors.same
    }, "target")
    val bridge = system.systemActorOf(BridgeActor(_ => Some(target)), "bridge")
    val admin = system.systemActorOf(Behaviors.receiveMessage[AdminPort.Msg] {
      case c: AdminPort.CmdIn =>
        effects.incrementAndGet()
        c.sink.send(c.cid, c.replyTo, AdminV2Codec.encodeReply(AdminV2Codec.AdminReplyV2.Stopped(1)))
        Behaviors.same
      case _ => Behaviors.same
    }, "admin-spy")
    val transport = new ParrotTransportExtension(system, bridge, "envelope", Some(admin))
    var socket: Socket = null
    var input: DataInputStream = null
    def start(): Unit = {
      transport.listen(0); reconnect()
    }
    def reconnect(): Unit = {
      if (socket != null) socket.close()
      socket = new Socket("127.0.0.1", transport.port)
      socket.setSoTimeout(1000); input = new DataInputStream(socket.getInputStream)
    }
    def send(ft: Byte, cid: Long, key: String, payload: Array[Byte], flags: Int = 0): Unit = {
      socket.getOutputStream.write(WireFrame.Frame(1, ft, flags, cid, 0, 8, "/user/test", key, payload).encode())
      socket.getOutputStream.flush()
    }
    def read(): Option[WireFrame.Frame] = {
      val first = input.read()
      if (first == -1) None
      else {
        val header = new Array[Byte](4); header(0) = first.toByte; input.readFully(header, 1, 3)
        val size = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN).getInt
        assert(size >= 28 && size <= 16 * 1024 * 1024,
          s"invalid frame prefix: ${header.map(b => f"${b & 0xff}%02x").mkString}")
        val body = new Array[Byte](size); input.readFully(body)
        Some(WireFrame.decode(header ++ body).get._1)
      }
    }
    def handshake(limit: Int = WireFrame.MAX_FRAME_LEN): Unit = {
      val hello = WireFrame.handshakeBody("envelope-test")
      var offset = 0
      var found = false
      while (offset < hello.length) {
        val tag = hello(offset)
        val length = (hello(offset + 1) & 0xff) | ((hello(offset + 2) & 0xff) << 8)
        if (tag == WireFrame.TlvTag.MAX_FRAME_LEN) {
          assert(length == 4)
          ByteBuffer.wrap(hello, offset + 3, 4).order(ByteOrder.LITTLE_ENDIAN).putInt(limit)
          found = true
        }
        offset += 3 + length
      }
      assert(found)
      send(HANDSHAKE, 0, "", hello)
      assert(read().get.frameType == HANDSHAKE_ACK)
    }
    def close(): Unit = {
      if (socket != null) socket.close()
      transport.shutdown().get(10, java.util.concurrent.TimeUnit.SECONDS)
      system.terminate(); Await.result(system.whenTerminated, 10.seconds)
    }
  }
}
