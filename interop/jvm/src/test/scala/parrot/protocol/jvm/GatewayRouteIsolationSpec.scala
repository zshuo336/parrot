package parrot.protocol.jvm

import akka.actor.typed.{ActorSystem, Behavior}
import akka.actor.typed.scaladsl.Behaviors
import org.scalatest.funsuite.AnyFunSuite
import java.io.DataInputStream
import java.net.Socket
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.TimeUnit
import scala.concurrent.Await
import scala.concurrent.duration._
import AdminV2Codec._
import AdminV2Codec.AdminCommandV2._
import AdminV2Codec.AdminReplyV2._
import AdminV2Executor._
import BridgeActor._

class RouteIsolationEcho extends ComponentSpi {
  def behavior(component: ComponentContext): Behavior[Any] = Behaviors.setup { context =>
    val marker = component.config.get
    def echo(suffix: String): Behavior[Any] = Behaviors.receiveMessage {
      case BridgeAsk(key, payload, to) =>
        to ! BridgeReplyOk(key, marker ++ suffix.getBytes("UTF-8") ++ payload); Behaviors.same
      case _ => Behaviors.same
    }
    context.spawn(echo("/child:"), "child")
    echo(":")
  }
}

class GatewayRouteIsolationSpec extends AnyFunSuite {
  class Gateway(val name: String, sharedSystem: Option[ActorSystem[Any]] = None,
                routeScope: Option[ComponentRoutes] = None) {
    val system = sharedSystem.getOrElse(ActorSystem[Any](Behaviors.empty, name))
    val routes = routeScope.getOrElse(ComponentRoutes(system))
    private var closed = false
    val bridge = system.systemActorOf(BridgeActor(routes.get,
      path => routes.resolveChild(path)(system.executionContext)), name + "-bridge")
    val admin = system.systemActorOf(AdminPort(system, routes), name + "-admin")
    val transport = new ParrotTransportExtension(system, bridge, name, Some(admin))
    transport.listen(0)
    val socket = new Socket("127.0.0.1", transport.port)
    socket.setSoTimeout(5000)
    var next = 0L
    def exchange(kind: Byte, path: String, key: String, bytes: Array[Byte]): WireFrame.Frame = {
      next += 1
      socket.getOutputStream.write(WireFrame.Frame(1, kind, 0, next, 0, 8, path, key, bytes).encode())
      socket.getOutputStream.flush()
      val input = new DataInputStream(socket.getInputStream)
      val header = new Array[Byte](4); input.readFully(header)
      val n = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN).getInt
      assert(n >= 28 && n <= WireFrame.MAX_FRAME_LEN)
      val body = new Array[Byte](n); input.readFully(body)
      val reply = WireFrame.decode(header ++ body).get._1
      if (kind != WireFrame.FrameType.HANDSHAKE) assert(reply.correlationId == next)
      reply
    }
    assert(exchange(WireFrame.FrameType.HANDSHAKE, "", "", WireFrame.handshakeBody("test-peer")).frameType == WireFrame.FrameType.HANDSHAKE_ACK)
    def command(build: Long => AdminCommandV2): AdminReplyV2 =
      decodeReply(exchange(WireFrame.FrameType.SYSTEM_EVENT, "/admin", "", encodeCommand(build(next + 1))).payload)
    def deploy(marker: String, componentName: String = "shared"): AdminReplyV2 = command(id => Deploy(id, ComponentDeploy(componentName, "v1",
      ArtifactRef.Jvm(classOf[RouteIsolationEcho].getName, None), InstancePolicy.Singleton, Some(marker.getBytes("UTF-8")))))
    def ask(path: String): WireFrame.Frame = exchange(WireFrame.FrameType.ASK, "/user/" + path, "test:echo",
      WireFrame.withReplyToPrefix("/receipt", "value".getBytes("UTF-8")))
    def expect(path: String, value: String): Unit = {
      val result = ask(path)
      assert(result.frameType == WireFrame.FrameType.REPLY && result.path == "/receipt")
      assert(new String(result.payload, "UTF-8") == value)
    }
    def stopAdmin(): Unit = {
      import akka.actor.typed.scaladsl.adapter._
      Await.result(akka.pattern.gracefulStop(admin.toClassic, 5.seconds), 6.seconds)
    }
    def close(): Unit = if (!closed) {
      closed = true
      socket.close()
      try transport.shutdown().get(10, TimeUnit.SECONDS)
      finally {
        if (sharedSystem.isEmpty) { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
        else stopAdmin()
      }
    }
  }
  def pair(run: (Gateway, Gateway) => Unit): Unit = {
    val a = new Gateway("scope-a")
    try { val b = new Gateway("scope-b"); try run(a, b) finally b.close() }
    finally a.close()
  }
  for (path <- Seq("shared", "shared/child")) {
    test(s"a gateway without a deployment cannot resolve another ActorSystem's $path") {
      pair { (a, b) =>
        assert(a.deploy("A").isInstanceOf[Deployed])
        a.expect(path, if (path.contains("/")) "A/child:value" else "A:value")
        val result = b.ask(path)
        assert(result.frameType == WireFrame.FrameType.REPLY_ERR,
          s"foreign component executed through ${b.name}: ${new String(result.payload, "UTF-8")}")
        assert(WireFrame.decodeErrPayload(result.payload)._1 == WireFrame.ErrCode.ActorNotFound)
      }
    }
  }
  test("same component names in distinct ActorSystems deploy replace drain and stop independently") {
    pair { (a, b) =>
      assert(a.deploy("A").isInstanceOf[Deployed])
      assert(b.deploy("B").isInstanceOf[Deployed])
      a.expect("shared", "A:value"); b.expect("shared", "B:value")
      a.expect("shared/child", "A/child:value"); b.expect("shared/child", "B/child:value")
      assert(a.deploy("A2").isInstanceOf[Deployed])
      a.expect("shared", "A2:value"); b.expect("shared", "B:value")
      assert(a.command(id => Drain(id, "/user/shared", 2000)).isInstanceOf[Drained])
      b.expect("shared/child", "B/child:value")
      assert(a.deploy("A3").isInstanceOf[Deployed])
      assert(a.command(id => Stop(id, "/user/shared")).isInstanceOf[Stopped])
      b.expect("shared", "B:value")
    }
  }

  test("terminating one system does not remove another system's same-named root or child") {
    pair { (a, b) =>
      assert(a.deploy("A").isInstanceOf[Deployed]); assert(b.deploy("B").isInstanceOf[Deployed])
      a.close()
      assert(a.routes.get("shared").isEmpty)
      b.expect("shared", "B:value"); b.expect("shared/child", "B/child:value")
    }
  }
  test("explicit scopes isolate same-named gateways within one ActorSystem") {
    val system = ActorSystem[Any](Behaviors.empty, "same-system-isolated")
    try {
      val a = new Gateway("a", Some(system), Some(ComponentRoutes.newScope(system)))
      try {
        val b = new Gateway("b", Some(system), Some(ComponentRoutes.newScope(system)))
        try {
          assert(a.deploy("A").isInstanceOf[Deployed]); assert(b.deploy("B").isInstanceOf[Deployed])
          a.expect("shared/child", "A/child:value"); b.expect("shared/child", "B/child:value")
          assert(a.deploy("A2").isInstanceOf[Deployed]); a.expect("shared", "A2:value")
          a.close()
          assert(a.routes.get("shared").isEmpty)
          b.expect("shared", "B:value"); b.expect("shared/child", "B/child:value")
        } finally b.close()
      } finally a.close()
    } finally { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
  }
  test("default scope is shared within a system and rejects another admin's route takeover") {
    val system = ActorSystem[Any](Behaviors.empty, "same-system-shared")
    try {
      val a = new Gateway("a", Some(system))
      try {
        val b = new Gateway("b", Some(system))
        try {
          assert(a.routes eq b.routes)
          assert(a.deploy("A").isInstanceOf[Deployed])
          b.deploy("B") match {
            case Failed(_, ErrSpawnFailed, _) => ()
            case other => fail(s"route takeover: $other")
          }
          b.expect("shared/child", "A/child:value")
          assert(b.deploy("B", "other").isInstanceOf[Deployed])
          a.close()
          assert(b.routes.get("shared").isEmpty)
          b.expect("other", "B:value"); b.expect("other/child", "B/child:value")
          assert(b.deploy("B2").isInstanceOf[Deployed])
          b.expect("shared", "B2:value")
        } finally b.close()
      } finally a.close()
    } finally { system.terminate(); Await.result(system.whenTerminated, 10.seconds) }
  }
}
