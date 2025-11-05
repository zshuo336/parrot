package parrot.protocol.jvm

import akka.actor.typed.{ActorSystem, Behavior, PostStop, SupervisorStrategy}
import akka.actor.typed.scaladsl.Behaviors
import io.opentelemetry.api.common.AttributeKey
import io.opentelemetry.api.trace.{Span, StatusCode}
import io.opentelemetry.sdk.trace.SdkTracerProvider
import io.opentelemetry.sdk.trace.export.SimpleSpanProcessor
import io.opentelemetry.sdk.trace.samplers.Sampler
import io.opentelemetry.sdk.testing.exporter.InMemorySpanExporter
import org.scalatest.funsuite.AnyFunSuite
import java.io.DataInputStream
import java.net.{ServerSocket, Socket}
import java.nio.{ByteBuffer, ByteOrder}
import java.util.concurrent.{LinkedBlockingQueue, TimeUnit}
import scala.concurrent.{Await, Promise}
import scala.concurrent.duration._
import scala.jdk.CollectionConverters._
import BridgeActor._
import WireFrame.FrameType._

class CausalTraceComponent extends ComponentSpi {
  override def behavior(context: ComponentContext): Behavior[Any] = Behaviors.receiveMessage {
    case BridgeAsk(key, _, reply) =>
      val value = CausalTrace.current.map(p => p.traceId + ":" + p.spanId).getOrElse("none")
      reply ! BridgeReplyOk(key, value.getBytes("UTF-8"))
      Behaviors.same
    case _ => Behaviors.same
  }
}

class CausalTraceSpec extends AnyFunSuite {
  val first = CausalTrace.Parent("11" * 16, "22" * 8)
  val second = CausalTrace.Parent("33" * 16, "44" * 8)

  case class Observed(parent: Option[CausalTrace.Parent], current: Option[CausalTrace.Parent],
                      payload: Vector[Byte], recording: Boolean)

  class Peer(val socket: Socket) extends AutoCloseable {
    socket.setSoTimeout(5000)
    val input = new DataInputStream(socket.getInputStream)
    def send(kind: Byte, payload: Array[Byte], parent: Option[CausalTrace.Parent] = None,
             flags: Int = 0, path: String = "/user/actor", cid: Long = 12): Unit = {
      val body = parent.fold(payload)(p => p.encode ++ payload)
      val frame = WireFrame.Frame(1, kind, flags | parent.fold(0)(_ => WireFrame.FLAG_TRACE_CONTEXT),
        cid, 0, 8, path, "test:observe", body)
      socket.getOutputStream.write(frame.encode())
      socket.getOutputStream.flush()
    }
    def read(): Option[WireFrame.Frame] = {
      val first = input.read()
      if (first < 0) None
      else {
        val header = Array(first.toByte) ++ input.readNBytes(3)
        assert(header.length == 4)
        val size = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN).getInt
        assert(size >= 28 && size <= WireFrame.MAX_FRAME_LEN)
        val body = input.readNBytes(size)
        assert(body.length == size)
        Some(WireFrame.decode(header ++ body).get._1)
      }
    }
    def handshake(capabilities: Int = WireFrame.CAPS_BIN_ONLY | WireFrame.CAPS_TRACE_CONTEXT): WireFrame.PeerHandshake = {
      send(HANDSHAKE, WireFrame.handshakeBody("trace-peer", capabilities), path = "")
      val reply = read().get
      assert(reply.frameType == HANDSHAKE_ACK)
      WireFrame.decodeHandshake(reply.payload, acknowledgement = true)
    }
    def ask(payload: Array[Byte], parent: Option[CausalTrace.Parent], path: String = "/user/actor"): WireFrame.Frame = {
      send(ASK, WireFrame.withReplyToPrefix("/receipt", payload), parent, path = path)
      val reply = read().get
      assert(reply.frameType == REPLY && reply.correlationId == 12 && reply.path == "/receipt")
      reply
    }
    override def close(): Unit = socket.close()
  }

  class Runtime(traceEnabled: Boolean = true, record: Boolean = true,
                resolver: Option[Promise[Option[akka.actor.typed.ActorRef[Any]]]] = None,
                limits: GatewayAskLimits = GatewayAskLimits(), targetName: String = "/user/actor") extends AutoCloseable {
    val exporter = InMemorySpanExporter.create()
    val provider = SdkTracerProvider.builder().setSampler(Sampler.alwaysOn())
      .addSpanProcessor(SimpleSpanProcessor.create(exporter)).build()
    val system = ActorSystem[Any](Behaviors.empty, "causal-" + java.util.UUID.randomUUID())
    if (record) CausalTrace(system).setTracer(provider.get("parrot-test"))
    val observed = new LinkedBlockingQueue[Observed]()
    val stopped = Promise[Option[CausalTrace.Parent]]()
    val resolveStarted = Promise[Unit]()
    val underlying = Behaviors.receiveMessage[Any] {
      case request @ BridgeAsk(key, payload, reply) =>
        observed.add(Observed(request.traceParent, CausalTrace.current, payload.toVector, Span.current().isRecording))
        reply ! BridgeReplyOk(key, payload)
        Behaviors.same
      case request @ BridgeTell(_, payload) =>
        observed.add(Observed(request.traceParent, CausalTrace.current, payload.toVector, Span.current().isRecording))
        if (payload.sameElements(Array[Byte](99))) throw new IllegalStateException("private business failure")
        Behaviors.same
      case _ => Behaviors.same
    }.receiveSignal { case (_, PostStop) => stopped.trySuccess(CausalTrace.current); Behaviors.same }
    val target = system.systemActorOf(Behaviors.supervise(CausalTrace.behavior(underlying, system, targetName))
      .onFailure[IllegalStateException](SupervisorStrategy.resume), "target")
    val routes = AdminV2Executor.ComponentRoutes(system)
    val bridge = system.systemActorOf(BridgeActor(path => routes.get(path).orElse(if (path == "actor") Some(target) else None),
      _ => { resolveStarted.trySuccess(()); resolver.map(_.future).getOrElse(scala.concurrent.Future.successful(None)) }), "bridge")
    val admin = system.systemActorOf(AdminPort(system), "admin")
    val transport = new ParrotTransportExtension(system, bridge, "causal", Some(admin), limits, traceEnabled)
    def connect(): Peer = new Peer(new Socket("127.0.0.1", transport.port))
    def next(): Observed = {
      val value = observed.poll(5, TimeUnit.SECONDS)
      assert(value != null, "actual handler did not execute")
      value
    }
    def spans = exporter.getFinishedSpanItems.asScala.toVector
    override def close(): Unit = {
      transport.shutdown().get(15, TimeUnit.SECONDS)
      system.terminate()
      Await.result(system.whenTerminated, 15.seconds)
      assert(Await.result(stopped.future, 5.seconds).isEmpty, "PostStop inherited a business parent")
      provider.forceFlush().join(5, TimeUnit.SECONDS)
      provider.close()
    }
  }

  def runtime(action: Runtime => Unit, tracing: Boolean = true, record: Boolean = true,
              resolver: Option[Promise[Option[akka.actor.typed.ActorRef[Any]]]] = None,
              limits: GatewayAskLimits = GatewayAskLimits(), targetName: String = "/user/actor"): Unit = {
    val owner = new Runtime(tracing, record, resolver, limits, targetName)
    try { owner.transport.listen(0); action(owner) } finally owner.close()
  }

  test("wire parent is fixed-size validated immutable identity") {
    val encoded = first.encode
    assert(encoded.length == 25)
    val (decoded, payload) = CausalTrace.Parent.split(encoded ++ Array[Byte](5, 6))
    encoded(1) = 0
    assert(decoded == first && payload.sameElements(Array[Byte](5, 6)))
    for (bytes <- Vector(Array.emptyByteArray, Array.fill[Byte](24)(1), Array.fill[Byte](25)(0),
                         Array(2.toByte) ++ first.encode.drop(1), Array(1.toByte) ++ new Array[Byte](16) ++ first.encode.takeRight(8),
                         first.encode.take(17) ++ new Array[Byte](8))) {
      intercept[IllegalArgumentException](CausalTrace.Parent.split(bytes))
    }
    intercept[IllegalArgumentException](CausalTrace.Parent("0" * 32, first.spanId))
    intercept[IllegalArgumentException](CausalTrace.Parent("G" * 32, first.spanId))
    intercept[IllegalArgumentException](CausalTrace.withParent(Some(null))(()))
    intercept[IllegalArgumentException](BridgeTell("key", Array.emptyByteArray, Some(null)))
  }

  test("message constructors extractors and copies preserve captured parent") {
    val payload = Array[Byte](1)
    val tell = CausalTrace.withParent(Some(first))(BridgeTell("key", payload))
    val BridgeTell(key, extracted) = tell
    assert(key == "key" && (extracted eq payload))
    assert(tell.traceParent.contains(first) && tell.copy(typeKey = "copy").traceParent.contains(first))
    assert(tell.copy("legacy", payload).traceParent.contains(first))
    assert(CausalTrace.withParent(Some(second))(new BridgeTell("key", payload)).traceParent.contains(second))
    assert(BridgeTell("key", payload).traceParent.isEmpty)
    val ask = CausalTrace.withParent(Some(first))(BridgeAsk("key", payload, null))
    val BridgeAsk(askKey, askPayload, reply) = ask
    assert(askKey == "key" && (askPayload eq payload) && reply == null)
    assert(ask.copy(typeKey = "copy").traceParent.contains(first))
    assert(ask.copy("legacy", payload, null).traceParent.contains(first))
    val internal = CausalTrace.withParent(Some(first))(Ask(7, "actor", "key", payload, null))
    val Ask(cid, path, _, _, _) = internal
    assert(cid == 7 && path == "actor" && internal.copy(cid = 8).traceParent.contains(first))
    val internalTell = CausalTrace.withParent(Some(first))(Tell("actor", "key", payload))
    assert(internalTell.copy(akkaPath = "other").traceParent.contains(first))
  }

  test("explicit empty scope and captured callbacks restore the invoking context on failure") {
    CausalTrace.withParent(Some(first)) {
      val callback = CausalTrace.capture(() => { assert(CausalTrace.current.contains(first)); throw new IllegalStateException("x") })
      CausalTrace.withParent(Some(second)) {
        intercept[IllegalStateException](callback())
        assert(CausalTrace.current.contains(second))
        CausalTrace.withParent(None) { assert(CausalTrace.current.isEmpty && !Span.current().getSpanContext.isValid) }
        assert(CausalTrace.current.contains(second))
      }
      assert(CausalTrace.current.contains(first))
    }
    assert(CausalTrace.current.isEmpty)
  }

  test("legacy bridge companions remain usable as FunctionN factories") {
    val tellFactory: Any = BridgeTell
    val askFactory: Any = BridgeAsk
    val internalTellFactory: Any = Tell
    val internalAskFactory: Any = Ask
    val payload = Array[Byte](14)
    CausalTrace.withParent(Some(first)) {
      assert(tellFactory.asInstanceOf[Function2[String, Array[Byte], BridgeTell]]
        .tupled(("tell", payload)).traceParent.contains(first))
      assert(askFactory.asInstanceOf[Function3[String, Array[Byte], akka.actor.typed.ActorRef[Any], BridgeAsk]]
        .curried("ask")(payload)(null).traceParent.contains(first))
      assert(internalTellFactory.asInstanceOf[Function3[String, String, Array[Byte], Tell]]
        .tupled(("actor", "tell", payload)).traceParent.contains(first))
      assert(internalAskFactory.asInstanceOf[Function5[Long, String, String, Array[Byte], akka.actor.typed.ActorRef[BridgeReply], Ask]]
        .tupled((1L, "actor", "ask", payload, null)).traceParent.contains(first))
    }
  }

  test("real TCP ASK TELL and untraced callback create only their actual child spans") {
    runtime({ owner =>
      val peer = owner.connect()
      try {
        assert((peer.handshake().capabilities & WireFrame.CAPS_TRACE_CONTEXT) != 0)
        assert(peer.ask(Array[Byte](4, 5), Some(first)).payload.sameElements(Array[Byte](4, 5)))
        peer.send(TELL, Array[Byte](6), Some(second))
        peer.ask(Array[Byte](7), None)
        val seen = Vector(owner.next(), owner.next(), owner.next())
        for ((value, parent) <- seen.take(2).zip(Vector(first, second))) {
          assert(value.parent.contains(parent) && value.current.exists(_.traceId == parent.traceId))
          assert(value.current.get.spanId != parent.spanId && value.recording)
        }
        assert(seen.map(_.payload) == Vector(Vector[Byte](4, 5), Vector[Byte](6), Vector[Byte](7)))
        assert(seen.last.parent.isEmpty && seen.last.current.isEmpty && !seen.last.recording)
        // Stop is a real callback-completion barrier, not merely receipt of the reply.
      } finally peer.close()
      owner.system.terminate(); Await.result(owner.system.whenTerminated, 15.seconds)
      val spans = owner.spans
      assert(spans.size == 2)
      assert(spans.map(s => (s.getTraceId, s.getParentSpanId)).toSet == Set((first.traceId, first.spanId), (second.traceId, second.spanId)))
      assert(spans.map(_.getAttributes.get(AttributeKey.stringKey("parrot.kind"))).toSet == Set("Ask", "Tell"))
    })
  }

  test("disabled recording still transfers identity and never inherits an untraced request") {
    runtime({ owner =>
      val peer = owner.connect()
      try {
        peer.handshake(); peer.ask(Array[Byte](1), Some(first)); peer.ask(Array[Byte](2), None)
        val traced = owner.next(); val plain = owner.next()
        assert(traced.parent.contains(first) && traced.current.contains(first) && !traced.recording)
        assert(plain.current.isEmpty && owner.spans.isEmpty)
      } finally peer.close()
    }, record = false)
  }

  test("causal prefix is included in actual gateway ASK admission bytes") {
    runtime({ owner =>
      val peer = owner.connect()
      try {
        peer.handshake()
        peer.send(ASK, WireFrame.withReplyToPrefix("/receipt", Array[Byte](1)), Some(first))
        val rejected = peer.read().get
        assert(rejected.frameType == REPLY_ERR)
        assert(WireFrame.decodeErrPayload(rejected.payload)._1 == WireFrame.ErrCode.Overloaded)
        peer.ask(Array[Byte](2), None)
        assert(owner.next().payload == Vector[Byte](2) && owner.observed.isEmpty)
      } finally peer.close()
    }, limits = GatewayAskLimits(bytes = 75, connectionBytes = 75))
  }

  test("actual SDK target attributes respect the UTF8 byte limit") {
    for (name <- Vector("a" * 256, "a" * 257, "\u6c49" * 85 + "x", "\u6c49" * 86)) {
      runtime({ owner =>
        val peer = owner.connect()
        try { peer.handshake(); peer.ask(Array[Byte](1), Some(first)); owner.next() } finally peer.close()
        owner.system.terminate(); Await.result(owner.system.whenTerminated, 15.seconds)
        val attributes = owner.spans.head.getAttributes
        val target = attributes.get(AttributeKey.stringKey("parrot.target"))
        assert(target.getBytes("UTF-8").length <= 256 && name.startsWith(target))
        assert(attributes.get(AttributeKey.booleanKey("parrot.target_truncated")).booleanValue() == (target != name))
      }, targetName = name)
    }
  }

  test("traced messages require both capabilities and cannot be used before handshake") {
    for ((enabled, caps, handshake) <- Vector((true, 1, true), (false, 8193, true), (true, 8193, false))) {
      runtime({ owner =>
        val peer = owner.connect()
        try {
          if (handshake) peer.handshake(caps)
          peer.send(TELL, Array[Byte](1), Some(first))
          assert(peer.read().isEmpty)
        } finally peer.close()
        val recovery = owner.connect()
        try { recovery.handshake(1); recovery.ask(Array[Byte](8), None) } finally recovery.close()
        assert(owner.next().payload == Vector[Byte](8))
        assert(owner.observed.isEmpty)
      }, tracing = enabled)
    }
  }

  test("invalid prefixes and traced control frames close before any handler dispatch") {
    runtime({ owner =>
      val invalid = Vector(first.encode.take(24), Array(2.toByte) ++ first.encode.drop(1),
        first.encode.take(17) ++ new Array[Byte](8), Array(1.toByte) ++ new Array[Byte](16) ++ first.encode.takeRight(8))
      for ((kind, bytes) <- invalid.map(TELL -> _) ++ Vector(HEARTBEAT -> first.encode, SYSTEM_EVENT -> first.encode)) {
        val peer = owner.connect()
        try {
          peer.handshake(); peer.send(kind, bytes, flags = WireFrame.FLAG_TRACE_CONTEXT)
          assert(peer.read().isEmpty)
        } finally peer.close()
      }
      val recovery = owner.connect()
      try { recovery.handshake(); recovery.ask(Array[Byte](9), None) } finally recovery.close()
      assert(owner.next().payload == Vector[Byte](9) && owner.observed.isEmpty && owner.spans.isEmpty)
    })
  }

  test("deferred child lookup retains the original parent after its enqueue scope ends") {
    val pending = Promise[Option[akka.actor.typed.ActorRef[Any]]]()
    runtime({ owner =>
      val peer = owner.connect()
      try {
        peer.handshake()
        peer.send(ASK, WireFrame.withReplyToPrefix("/receipt", Array[Byte](3)), Some(first), path = "/user/actor/child")
        Await.result(owner.resolveStarted.future, 5.seconds)
        CausalTrace.withParent(Some(second))(pending.success(Some(owner.target)))
        assert(peer.read().get.frameType == REPLY)
        val seen = owner.next()
        assert(seen.parent.contains(first) && seen.current.exists(_.traceId == first.traceId))
      } finally peer.close()
    }, resolver = Some(pending))
  }

  test("handler panic records an error and resumed actor clears the following request") {
    runtime({ owner =>
      val peer = owner.connect()
      try {
        peer.handshake(); peer.send(TELL, Array[Byte](99), Some(first)); peer.ask(Array[Byte](10), None)
        assert(owner.next().current.exists(_.traceId == first.traceId))
        assert(owner.next().current.isEmpty)
      } finally peer.close()
      owner.system.terminate(); Await.result(owner.system.whenTerminated, 15.seconds)
      val spans = owner.spans
      assert(spans.size == 1 && spans.head.getStatus.getStatusCode == StatusCode.ERROR)
      assert(spans.head.getAttributes.get(AttributeKey.stringKey("parrot.outcome")) == "Panicked")
      assert(spans.head.getEvents.isEmpty, "private exception text must not be captured")
    })
  }

  test("registration-mode actual TCP negotiates the same parent contract") {
    val listener = new ServerSocket(0)
    val owner = new Runtime()
    listener.setSoTimeout(5000)
    try {
      owner.transport.registerTo("127.0.0.1", listener.getLocalPort)
      val peer = new Peer(listener.accept())
      try {
        val hello = peer.read().get
        assert(hello.frameType == HANDSHAKE)
        assert((WireFrame.decodeHandshake(hello.payload, false).capabilities & WireFrame.CAPS_TRACE_CONTEXT) != 0)
        peer.send(HANDSHAKE_ACK, WireFrame.handshakeAckBody("hub", 8193), path = "")
        assert(peer.ask(Array[Byte](11), Some(first)).payload.sameElements(Array[Byte](11)))
        assert(owner.next().current.exists(_.traceId == first.traceId))
        owner.transport.shutdown().get(15, TimeUnit.SECONDS)
      } finally peer.close()
    } finally { owner.close(); listener.close() }
  }

  test("managed deployed component records its real implementation callback") {
    import AdminV2Codec._
    import AdminV2Codec.AdminCommandV2._
    runtime({ owner =>
      val peer = owner.connect()
      try {
        peer.handshake()
        peer.send(SYSTEM_EVENT, encodeCommand(Deploy(12, ComponentDeploy("traced", "1",
          ArtifactRef.Jvm(classOf[CausalTraceComponent].getName, None), InstancePolicy.Singleton, None))), path = "/admin")
        assert(decodeReply(peer.read().get.payload).isInstanceOf[AdminReplyV2.Deployed])
        val reply = peer.ask(Array.emptyByteArray, Some(first), "/user/traced")
        val values = new String(reply.payload, "UTF-8").split(":")
        assert(values.length == 2 && values(0) == first.traceId && values(1) != first.spanId)
        owner.system.terminate(); Await.result(owner.system.whenTerminated, 15.seconds)
        val spans = owner.spans
        assert(spans.size == 1 && spans.head.getParentSpanId == first.spanId)
        assert(spans.head.getAttributes.get(AttributeKey.stringKey("parrot.target")) == "/user/traced")
      } finally peer.close()
    })
  }

  test("a deferred callback sends the actual first handler child to the next actor") {
    runtime({ owner =>
      val callbacks = new LinkedBlockingQueue[() => Unit]()
      val captured = new LinkedBlockingQueue[Option[CausalTrace.Parent]]()
      val downstream = owner.system.systemActorOf(CausalTrace.behavior(Behaviors.receiveMessage[Any] {
        case request: BridgeTell =>
          captured.add(CausalTrace.current)
          owner.observed.add(Observed(request.traceParent, CausalTrace.current, request.payload.toVector,
            Span.current().isRecording))
          Behaviors.same
        case _ => Behaviors.same
      }, owner.system, "/user/downstream"), "downstream")
      val upstream = owner.system.systemActorOf(CausalTrace.behavior(Behaviors.receiveMessage[Any] {
        case request: BridgeTell =>
          captured.add(CausalTrace.current)
          callbacks.add(CausalTrace.capture(() => { downstream ! BridgeTell("next", request.payload); () }))
          Behaviors.same
        case _ => Behaviors.same
      }, owner.system, "/user/upstream"), "upstream")
      upstream ! BridgeTell("first", Array[Byte](41), Some(first))
      val callback = callbacks.poll(5, TimeUnit.SECONDS)
      assert(callback != null)
      val firstChild = captured.poll(5, TimeUnit.SECONDS)
      assert(firstChild.exists(p => p.traceId == first.traceId && p.spanId != first.spanId))
      CausalTrace.withParent(Some(second)) {
        callback()
        assert(CausalTrace.current.contains(second), "deferred callback must restore its caller")
      }
      val next = owner.next()
      assert(next.parent == firstChild && next.current.exists(p => p.traceId == first.traceId && p != firstChild.get))
      assert(next.payload == Vector[Byte](41) && next.recording)
      captured.poll(5, TimeUnit.SECONDS)
      CausalTrace.withParent(Some(second)) { upstream ! BridgeTell("plain", Array[Byte](42), None) }
      val plainCallback = callbacks.poll(5, TimeUnit.SECONDS)
      assert(plainCallback != null && captured.poll(5, TimeUnit.SECONDS).isEmpty)
      CausalTrace.withParent(Some(second))(plainCallback())
      assert(owner.next().current.isEmpty && captured.poll(5, TimeUnit.SECONDS).isEmpty)
      owner.system.terminate(); Await.result(owner.system.whenTerminated, 15.seconds)
      val spans = owner.spans
      assert(spans.size == 2)
      assert(spans.exists(s => s.getParentSpanId == first.spanId && s.getSpanId == firstChild.get.spanId))
      assert(spans.exists(s => s.getParentSpanId == firstChild.get.spanId && s.getTraceId == first.traceId))
    })
  }

  test("traced encoder rejection releases retained payload before any transport write") {
    import io.netty.buffer.{ByteBuf, Unpooled}
    import io.netty.channel.embedded.EmbeddedChannel
    for ((negotiated, kind, payload) <- Vector(
      (false, TELL, first.encode), (true, REPLY, first.encode),
      (true, TELL, first.encode.take(24)), (true, TELL, Array.fill[Byte](25)(0)))) {
      val channel = new EmbeddedChannel(new WireEncoder())
      channel.attr(WireFrame.TraceContextNegotiated).set(java.lang.Boolean.valueOf(negotiated))
      val buffer = Unpooled.wrappedBuffer(payload)
      val frame = new WireFrame.Frame(1, kind, WireFrame.FLAG_TRACE_CONTEXT, 1, 0, 8, "", "", null, buffer)
      val promise = channel.newPromise()
      try {
        channel.pipeline().writeAndFlush(frame, promise)
        channel.runPendingTasks()
        assert(promise.isDone && !promise.isSuccess && !channel.isActive)
        assert(buffer.refCnt() == 0 && channel.readOutbound[ByteBuf]() == null)
      } finally { frame.releaseBuf(); channel.finishAndReleaseAll() }
    }
  }

  test("component-bundled OpenTelemetry API and context share their host class identity") {
    val jar = java.nio.file.Files.createTempFile("parrot-duplicate-otel-", ".jar")
    try {
      val types = Vector(classOf[io.opentelemetry.context.Context], classOf[io.opentelemetry.api.trace.Span])
      val output = new java.util.jar.JarOutputStream(java.nio.file.Files.newOutputStream(jar))
      try types.foreach { value =>
        val resource = value.getName.replace('.', '/') + ".class"
        output.putNextEntry(new java.util.jar.JarEntry(resource))
        val input = value.getClassLoader.getResourceAsStream(resource)
        assert(input != null)
        try input.transferTo(output) finally input.close()
        output.closeEntry()
      } finally output.close()
      val loader = AdminV2Executor.buildLoader(AdminV2Codec.ArtifactRef.Jvm("fixture", None, Some(jar.toUri.toString))).get
      try types.foreach { value =>
        assert(loader.loadClass(value.getName) eq value, "component must share host trace API: " + value.getName)
      } finally loader.close()
    } finally java.nio.file.Files.deleteIfExists(jar)
  }

  test("traced retained-buffer encoding validates only the fixed-size prefix") {
    import io.netty.buffer.{ByteBuf, Unpooled, UnpooledByteBufAllocator}
    import io.netty.channel.embedded.EmbeddedChannel
    val meter = java.lang.management.ManagementFactory.getThreadMXBean match {
      case value: com.sun.management.ThreadMXBean if value.isThreadAllocatedMemorySupported => value
      case _ => fail("resource contract requires per-thread allocation accounting")
    }
    if (!meter.isThreadAllocatedMemoryEnabled) meter.setThreadAllocatedMemoryEnabled(true)
    def encode(size: Int): Long = {
      val channel = new EmbeddedChannel(new WireEncoder())
      channel.config().setAllocator(new UnpooledByteBufAllocator(true))
      channel.attr(WireFrame.TraceContextNegotiated).set(java.lang.Boolean.TRUE)
      val input = Unpooled.directBuffer(size).writeBytes(first.encode).writeZero(size - 25)
      val frame = new WireFrame.Frame(1, TELL, WireFrame.FLAG_TRACE_CONTEXT, 1, 0, 8, "", "", null, input)
      try {
        val thread = Thread.currentThread().threadId()
        val before = meter.getThreadAllocatedBytes(thread)
        channel.writeOutbound(frame)
        val allocated = meter.getThreadAllocatedBytes(thread) - before
        val output = channel.readOutbound[ByteBuf]()
        assert(output != null && output.readableBytes() == size + 32)
        try {
          assert(output.getByte(32) == 1 && output.getByte(56) == first.encode.last)
          assert(output.getByte(output.writerIndex() - 1) == 0)
        } finally output.release()
        assert(input.refCnt() == 0)
        allocated
      } finally { frame.releaseBuf(); channel.finishAndReleaseAll() }
    }
    for (_ <- 0 until 8) encode(64)
    val allocated = encode(4 * 1024 * 1024)
    assert(allocated >= 0 && allocated < 128 * 1024, s"fixed-prefix validation allocated $allocated heap bytes")
  }
}
