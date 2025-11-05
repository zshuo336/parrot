package parrot.protocol.jvm

import akka.actor.typed.{ActorRef, ActorSystem}
import akka.actor.typed.scaladsl.AskPattern._
import BridgeActor._
import io.netty.bootstrap.ServerBootstrap
import io.netty.buffer.{ByteBuf, Unpooled}
import io.netty.channel.nio.NioEventLoopGroup
import io.netty.channel.socket.SocketChannel
import io.netty.channel.socket.nio.NioServerSocketChannel
import io.netty.channel.{ChannelHandlerContext, ChannelInitializer, SimpleChannelInboundHandler}
import io.netty.handler.codec.ByteToMessageDecoder
import io.netty.util.concurrent.DefaultThreadFactory
import scala.concurrent.duration._
import scala.util.{Failure, Success}

/** Netty TCP server：说 Wire 1.0（含 golden vectors 兼容的字节级编解码）。
  *
  * 连接时序（与 parrot-remote run_connection 对偶）：
  * accept → 收 HANDSHAKE → 回 HANDSHAKE_ACK → 双向帧流。
  * 路径映射：/jvm/user/{akkaPath} → system.user/{akkaPath}。
  */
final class ParrotTransportExtension(
    system: ActorSystem[_],
    bridge: ActorRef[BridgeMsg],
    nodeId: String,
    adminPort: Option[ActorRef[AdminPort.Msg]] = None,
    askLimits: GatewayAskLimits = GatewayAskLimits(),
    traceContext: Boolean = false
) {
  def this(system: ActorSystem[_], bridge: ActorRef[BridgeMsg], nodeId: String,
           adminPort: Option[ActorRef[AdminPort.Msg]], askLimits: GatewayAskLimits) =
    this(system, bridge, nodeId, adminPort, askLimits, false)
  require(system != null || bridge == null, "business transport requires its owning ActorSystem")
  private val askBudget = Option(system).fold(new GatewayAskBudget(askLimits))(GatewayAskBudget.owned(_, askLimits))
  private[jvm] def pendingAsks: (Int, Long) = askBudget.snapshot
  private val scheduler = Option(system).map(_.scheduler).orNull
  private var group: NioEventLoopGroup = _
  private val channels = new io.netty.channel.group.DefaultChannelGroup(io.netty.util.concurrent.ImmediateEventExecutor.INSTANCE)
  private val stopped = new java.util.concurrent.atomic.AtomicBoolean(false)
  private val retired = new java.util.concurrent.CompletableFuture[Void]()
  @volatile private var retry: io.netty.util.concurrent.ScheduledFuture[_] = _
  @volatile private var boundPort: Int = -1
  private var ownerRegistration: Option[SystemRetirement#Registration] = None

  Option(system).foreach { owner =>
    val registration = SystemRetirement(owner).register(() => { shutdown(); () })
    synchronized {
      if (stopped.get()) registration.close() else ownerRegistration = Some(registration)
    }
  }

  def port: Int = boundPort

  private def startGroup(): NioEventLoopGroup = {
    require(group == null && !stopped.get(), "transport already started or stopped")
    group = new NioEventLoopGroup(1, new DefaultThreadFactory("parrot-jvm-netty"))
    group
  }

  private def pipeline(ch: SocketChannel, client: Boolean): Unit = {
    channels.add(ch)
    ch.pipeline()
      .addLast(new io.netty.handler.timeout.IdleStateHandler(10, 0, 0))
      .addLast(new WireDecoder())
      .addLast(new WireEncoder())
      .addLast(if (client) new ParrotClientHandler(bridge, nodeId, adminPort, scheduler, askBudget, traceContext)
        else new ParrotServerHandler(bridge, nodeId, adminPort, scheduler, askBudget, traceContext))
    if (stopped.get()) { WireConnection.mark(ch); ch.close() }
  }

  def listen(port0: Int): Unit = synchronized {
    val owner = startGroup()
    try {
      val bound = new ServerBootstrap().group(owner).channel(classOf[NioServerSocketChannel])
        // Netty defaults to SO_REUSEADDR. On BSD/macOS that can let a wildcard
        // bind report success while a specific-address listener owns its port;
        // clients then reach that unrelated service. Require endpoint ownership.
        .option(io.netty.channel.ChannelOption.SO_REUSEADDR, java.lang.Boolean.FALSE)
        .childHandler(new ChannelInitializer[SocketChannel] {
          override def initChannel(ch: SocketChannel): Unit = pipeline(ch, client = false)
        }).bind(port0).sync().channel()
      channels.add(bound)
      boundPort = bound.localAddress().asInstanceOf[java.net.InetSocketAddress].getPort
    } catch { case scala.util.control.NonFatal(error) => shutdown(); throw error }
  }

  /** One owned reconnect chain. Only closeFuture schedules the next attempt,
    * including a failed connect; retries reset only after a valid Wire handshake. */
  def registerTo(parrotHost: String, parrotPort: Int): Unit = synchronized {
    require(parrotHost.nonEmpty && parrotPort > 0 && parrotPort <= 65535, "invalid registration address")
    val owner = startGroup()
    val loop = owner.next()
    var attempt = 0
    def dial(): Unit = if (!stopped.get()) {
      val connection = new io.netty.bootstrap.Bootstrap().group(owner)
        .channel(classOf[io.netty.channel.socket.nio.NioSocketChannel])
        .option(io.netty.channel.ChannelOption.CONNECT_TIMEOUT_MILLIS, Integer.valueOf(10000))
        .handler(new ChannelInitializer[SocketChannel] {
          override def initChannel(ch: SocketChannel): Unit = pipeline(ch, client = true)
        }).connect(parrotHost, parrotPort)
      channels.add(connection.channel())
      connection.channel().closeFuture().addListener { (_: io.netty.channel.ChannelFuture) =>
        if (!stopped.get()) {
          if (java.lang.Boolean.TRUE == connection.channel().attr(WireFrame.HandshakeComplete).get()) attempt = 0
          val delay = math.min(60L, 1L << math.min(attempt, 6))
          attempt = math.min(attempt + 1, 6)
          retry = loop.schedule(new Runnable { override def run(): Unit = dial() }, delay, java.util.concurrent.TimeUnit.SECONDS)
        }
      }
    }
    loop.execute(new Runnable { override def run(): Unit = dial() })
  }

  /** Completion means every owned channel and Netty event loop has retired. */
  def shutdown(): java.util.concurrent.CompletableFuture[Void] = synchronized {
    if (stopped.compareAndSet(false, true)) {
      ownerRegistration.foreach(_.close()); ownerRegistration = None
      askBudget.close()
      if (retry != null) retry.cancel(false)
      val active = channels.iterator()
      while (active.hasNext) WireConnection.mark(active.next())
      channels.close()
      if (group == null) retired.complete(null)
      else group.shutdownGracefully(0, 10, java.util.concurrent.TimeUnit.SECONDS)
        .addListener { (result: io.netty.util.concurrent.Future[AnyRef]) =>
          if (result.isSuccess) retired.complete(null) else retired.completeExceptionally(result.cause())
          ()
        }
    }
    retired
  }
}

/** 半包解码器：直接在 ByteBuf 上解析（DEV_08 零拷贝——跳过旧实现
  * readBytes→Array→WireFrame.decode 的中转拷贝）。语义复刻 Frame::decode：
  * 不足一帧不推进 readerIndex。
  */
final class WireDecoder extends ByteToMessageDecoder {
  override def decode(ctx: ChannelHandlerContext, in: ByteBuf, out: java.util.List[Object]): Unit = {
    try {
      if (WireConnection.isTerminal(ctx)) {
        in.skipBytes(in.readableBytes())
        return
      }
      val limit = ctx.channel().attr(WireFrame.PeerFrameLimit).get()
      if (limit != null && in.readableBytes() >= 4)
        require(in.getUnsignedIntLE(in.readerIndex()) <= limit.longValue(), "frame exceeds negotiated limit")
      WireFrame.decodeBuffer(in).foreach { case (frame, consumed) =>
        in.skipBytes(consumed)
        val c = GatewayMetrics.counters
        GatewayMetrics.add(c.bytesRx, consumed.toLong)
        frame.frameType match {
          case WireFrame.FrameType.ASK => GatewayMetrics.add(c.asksRx, 1)
          case WireFrame.FrameType.TELL => GatewayMetrics.add(c.tellsRx, 1)
          case WireFrame.FrameType.HEARTBEAT => GatewayMetrics.add(c.heartbeatsRx, 1)
          case _ => ()
        }
        out.add(frame)
      }
    } catch {
      case _: IllegalArgumentException =>
        in.skipBytes(in.readableBytes())
        WireConnection.close(ctx)
    }
  }
}

/** Writes directly to Netty output and retires the owned payload even on failure. */
final class WireEncoder extends io.netty.handler.codec.MessageToByteEncoder[WireFrame.Frame] {
  override def write(ctx: ChannelHandlerContext, value: Any, promise: io.netty.channel.ChannelPromise): Unit = {
    value match {
      case frame: WireFrame.Frame =>
        // Observe even void writes before inspecting application-owned metadata.
        // A failed frame is terminal for this byte stream: callers must not stay
        // pending on a connection that silently dropped their response.
        val observed = promise.unvoid()
        observed.addListener { (result: io.netty.channel.ChannelFuture) =>
          if (!result.isSuccess) WireConnection.close(ctx)
        }
        if (WireConnection.isTerminal(ctx)) {
          frame.releaseBuf()
          observed.tryFailure(new java.nio.channels.ClosedChannelException())
          return
        }
        try {
          if ((frame.flags & WireFrame.FLAG_TRACE_CONTEXT) != 0) {
            require(ctx.channel().attr(WireFrame.TraceContextNegotiated).get() == java.lang.Boolean.TRUE &&
                    (frame.frameType == WireFrame.FrameType.ASK || frame.frameType == WireFrame.FrameType.TELL),
                    "causal prefix requires negotiated business request")
            CausalTrace.Parent.decodePrefix(frame.payloadPrefix(25))
          }
          val size = 4L + WireFrame.BODY_FIXED_OVERHEAD + frame.path.getBytes(java.nio.charset.StandardCharsets.UTF_8).length +
            frame.typeKey.getBytes(java.nio.charset.StandardCharsets.UTF_8).length + frame.payloadLength
          val ft = frame.frameType
          observed.addListener { (result: io.netty.channel.ChannelFuture) =>
            if (result.isSuccess) {
              val c = GatewayMetrics.counters
              GatewayMetrics.add(c.bytesTx, size)
              if (ft == WireFrame.FrameType.REPLY || ft == WireFrame.FrameType.REPLY_ERR)
                GatewayMetrics.add(c.repliesTx, 1)
              if (ft == WireFrame.FrameType.REPLY_ERR) GatewayMetrics.add(c.replyErrs, 1)
            }
          }
          super.write(ctx, value, observed)
        } catch {
          case scala.util.control.NonFatal(error) =>
            frame.releaseBuf()
            observed.tryFailure(error)
        }
      case _ => super.write(ctx, value, promise)
    }
  }

  override def encode(ctx: ChannelHandlerContext, msg: WireFrame.Frame, out: ByteBuf): Unit =
    try {
      WireFrame.writeFrame(msg, out)
      if (ctx != null) {
        val limit = ctx.channel().attr(WireFrame.PeerFrameLimit).get()
        require(limit == null || out.readableBytes().toLong - 4 <= limit.longValue(), "frame exceeds peer limit")
      }
    } finally msg.releaseBuf()
}

class ParrotServerHandler(
    bridge: ActorRef[BridgeMsg],
    nodeId: String,
    adminPort: Option[ActorRef[AdminPort.Msg]] = None,
    scheduler: akka.actor.typed.Scheduler = null,
    askBudget: GatewayAskBudget = new GatewayAskBudget(),
    traceContext: Boolean = false
)
    extends SimpleChannelInboundHandler[WireFrame.Frame] {
  def this(bridge: ActorRef[BridgeMsg], nodeId: String, adminPort: Option[ActorRef[AdminPort.Msg]],
           scheduler: akka.actor.typed.Scheduler, askBudget: GatewayAskBudget) =
    this(bridge, nodeId, adminPort, scheduler, askBudget, false)
  def this(bridge: ActorRef[BridgeMsg], nodeId: String, adminPort: Option[ActorRef[AdminPort.Msg]], system: ActorSystem[_]) = {
    this(bridge, nodeId, adminPort, system.scheduler, GatewayAskBudget.owned(system))
    ownAskBudget()
  }

  require(bridge == null || scheduler != null, "business handler requires its owning ActorSystem scheduler")
  require(bridge == null || askBudget.ownerBound, "business handler requires an ActorSystem-owned ASK budget")
  /** 子类（注册模式 client handler）可见——客户端握手完成后置位复用分发。 */

  protected def gatewayCapabilities: Int = WireFrame.CAPS_BIN_ONLY |
    (if (adminPort.isDefined) WireFrame.CAPS_MEASURED else 0) |
    (if (traceContext) WireFrame.CAPS_TRACE_CONTEXT else 0)

  protected final def negotiatePeer(ctx: ChannelHandlerContext, peer: WireFrame.PeerHandshake): Unit = {
    ctx.channel().attr(WireFrame.PeerFrameLimit).set(
      java.lang.Long.valueOf(math.min(peer.maxFrameLen, WireFrame.MAX_FRAME_LEN.toLong)))
    ctx.channel().attr(WireFrame.TraceContextNegotiated).set(java.lang.Boolean.valueOf(
      (peer.capabilities & gatewayCapabilities & WireFrame.CAPS_TRACE_CONTEXT) != 0))
  }

  private def requestPayload(message: WireFrame.Frame): (Option[CausalTrace.Parent], Array[Byte]) =
    if ((message.flags & WireFrame.FLAG_TRACE_CONTEXT) == 0) (None, message.payload)
    else { val (parent, payload) = CausalTrace.Parent.split(message.payload); (Some(parent), payload) }

  private val asks = askBudget.connection()
  private var ownsBudget = false
  protected final def ownAskBudget(): Unit = { ownsBudget = true }
  private var started = false
  private var retired = false
  private var negotiating = false
  private var handshakeTimeout: io.netty.util.concurrent.ScheduledFuture[_] = _
  protected var handshaken = false
  protected def handshakeTimeoutMillis: Long = 10000L

  override def channelActive(ctx: ChannelHandlerContext): Unit = {
    started = true
    handshakeTimeout = ctx.executor().schedule(new Runnable {
      override def run(): Unit = if (!handshaken) WireConnection.close(ctx)
    }, handshakeTimeoutMillis, java.util.concurrent.TimeUnit.MILLISECONDS)
    super.channelActive(ctx)
  }

  protected def completeHandshake(ctx: ChannelHandlerContext): Unit = {
    if (!retired && !handshaken && !WireConnection.isTerminal(ctx)) {
      handshaken = true
      ctx.channel().attr(WireFrame.HandshakeComplete).set(java.lang.Boolean.TRUE)
      if (handshakeTimeout != null) handshakeTimeout.cancel(false)
      GatewayMetrics.add(GatewayMetrics.counters.handshakesOk, 1)
      GatewayMetrics.counters.connections.incrementAndGet()
    }
  }

  private def retireConnection(): Unit = {
    if (!retired) {
      retired = true
      asks.close()
      if (ownsBudget) askBudget.close()
      if (handshakeTimeout != null) handshakeTimeout.cancel(false)
      if (handshaken) GatewayMetrics.counters.connections.decrementAndGet()
      else if (started) GatewayMetrics.add(GatewayMetrics.counters.handshakesFailed, 1)
    }
  }
  override def channelInactive(ctx: ChannelHandlerContext): Unit = {
    WireConnection.mark(ctx.channel())
    retireConnection()
    super.channelInactive(ctx)
  }
  override def handlerRemoved(ctx: ChannelHandlerContext): Unit = {
    WireConnection.close(ctx)
    retireConnection()
    super.handlerRemoved(ctx)
  }
  final override def channelRead0(ctx: ChannelHandlerContext, msg: WireFrame.Frame): Unit =
    try {
      // Already decoded frames may still arrive during close/decodeLast. Always
      // retire their payload, but never resume protocol or application dispatch.
      if (retired || WireConnection.isTerminal(ctx)) return
      // Structural decoding preserves flags; admission must reject payload
      // formats this gateway cannot execute, including before handshake ACK.
      val causal = (msg.flags & WireFrame.FLAG_TRACE_CONTEXT) != 0
      val supportedMetadata = 0x0002 | 0x0008 | (if (handshaken &&
        ctx.channel().attr(WireFrame.TraceContextNegotiated).get() == java.lang.Boolean.TRUE) WireFrame.FLAG_TRACE_CONTEXT else 0)
      if ((msg.flags & ~supportedMetadata) != 0 || msg.typeKey == "parrot:durable#v1" ||
          (causal && msg.frameType != WireFrame.FrameType.ASK && msg.frameType != WireFrame.FrameType.TELL)) WireConnection.close(ctx)
      else handleFrame(ctx, msg)
    } finally msg.releaseBuf()

  protected def handleFrame(ctx: ChannelHandlerContext, msg: WireFrame.Frame): Unit = {
    import WireFrame.FrameType
    if (!handshaken) {
      msg.frameType match {
        case FrameType.HANDSHAKE if !negotiating =>
          val peer = WireFrame.decodeHandshake(msg.payload, acknowledgement = false)
          negotiating = true
          ctx.writeAndFlush(
            WireFrame.Frame(1, FrameType.HANDSHAKE_ACK, 0, 0, 0, 8, "", "", WireFrame.handshakeAckBody(nodeId, gatewayCapabilities))
          ).addListener { (result: io.netty.channel.ChannelFuture) =>
            if (result.isSuccess && !WireConnection.isTerminal(ctx)) {
              // ACK is a handshake frame and uses the global bound. Both
              // established directions use the negotiated minimum afterwards.
              negotiatePeer(ctx, peer)
              completeHandshake(ctx)
            } else WireConnection.close(ctx)
          }
        case _ => WireConnection.close(ctx) // 未握手先数据 = 协议违规
      }
      return
    }
    msg.frameType match {
      case FrameType.ASK =>
        // Two outstanding requests cannot own one wire correlation. Do not emit
        // a second receipt that the peer could mistake for the first request.
        if (asks.owns(msg.correlationId)) { WireConnection.close(ctx); return }
        // Admin deployment publishes /user/...; retain the older /jvm/user/... alias.
        val akkaPath = ParrotServerHandler.actorPath(msg.path)
        val (parent, payload) = requestPayload(msg)
        val (replyTo, realPayload) = WireFrame.splitReplyTo(payload) match {
          case Some(value) => value
          case None =>
            writeResponse(ctx)(WireFrame.Frame(1, FrameType.REPLY_ERR, 0, msg.correlationId, 0, 8, "", "",
              WireFrame.encodeErrPayload(WireFrame.ErrCode.ProtocolViolation, "invalid ASK reply_to prefix")))
            return
        }
        val cid = msg.correlationId
        val key = msg.typeKey
        val weight = realPayload.length.toLong + 2L * (akkaPath.length.toLong + key.length + replyTo.length) + parent.fold(0)(_ => 25)
        asks.acquire(cid, weight) match {
          case Left(GatewayAskBudget.Overloaded) =>
            writeResponse(ctx)(WireFrame.Frame(1, FrameType.REPLY_ERR, 0, cid, 0, 8, replyTo, "",
              WireFrame.encodeErrPayload(WireFrame.ErrCode.Overloaded, "gateway ASK capacity exhausted")))
          case Left(_) => WireConnection.close(ctx)
          case Right(ticket) =>
            // Do not capture msg: it may own an inbound ByteBuf or a second copy
            // of the request. Terminal callbacks need only the receipt metadata.
            def failed(error: Throwable): Unit = ticket.complete {
              writeResponse(ctx)(WireFrame.Frame(1, FrameType.REPLY_ERR, 0, cid, 0, 8, replyTo, "",
                WireFrame.encodeErrPayload(WireFrame.ErrCode.Timeout,
                  Option(error.getMessage).getOrElse("ask failed"))))
            }
            try {
              val future = bridge.ask[BridgeReply](
                ref => Ask(cid, akkaPath, key, realPayload, ref, parent)
              )(ParrotServerHandler.askTimeout, scheduler)
              future.onComplete {
                case Success(Replied(_, responseKey, payload)) => ticket.complete {
                  writeResponse(ctx)(WireFrame.Frame(1, FrameType.REPLY, 0, cid, 0, 8, replyTo, responseKey, payload))
                }
                case Success(ReplyErr(_, code, detail)) => ticket.complete {
                  writeResponse(ctx)(WireFrame.Frame(1, FrameType.REPLY_ERR, 0, cid, 0, 8, replyTo, "",
                    WireFrame.encodeErrPayload(code, detail)))
                }
                case Failure(error) => failed(error)
              }(scala.concurrent.ExecutionContext.parasitic)
            } catch { case scala.util.control.NonFatal(error) => failed(error) }
        }
      case FrameType.TELL =>
        val akkaPath = ParrotServerHandler.actorPath(msg.path)
        val (parent, payload) = requestPayload(msg)
        bridge ! Tell(akkaPath, msg.typeKey, payload, parent)
      case FrameType.STOP =>
        writeResponse(ctx)(
          WireFrame.Frame(
            1, FrameType.REPLY_ERR, 0, msg.correlationId, 0, 8, "", "",
            WireFrame.encodeErrPayload(WireFrame.ErrCode.Forbidden, "akka stop unsupported (I4)")
          )
        )
      case FrameType.HEARTBEAT =>
        writeResponse(ctx)(WireFrame.Frame(1, FrameType.HEARTBEAT_ACK, 0, 0, 0, 8, "", "", Array.emptyByteArray))
      case FrameType.SYSTEM_EVENT =>
        // B5（DEV_09）：admin-v2（tag 0x03）→ AdminPort 执行回 0x04 回执帧；
        // 其它 tag（gossip 等）吞帧不断连（与 ray/erl 方言一致）。
        val payload = msg.payload
        if (payload.nonEmpty && (payload(0) & 0xFF) == AdminV2Codec.TagAdminCmdV2) {
          adminPort match {
            case Some(port) =>
              // The callback belongs to this request, even when an embedded
              // AdminPort supplies different receipt metadata or completes twice
              // (for example success followed by a worker-retirement failure).
              // Capture only routing metadata, not the inbound frame/payload.
              val requestCid = msg.correlationId
              val requestPath = msg.path
              val sink = new AdminPort.ReplySink {
                private val completed = new java.util.concurrent.atomic.AtomicBoolean(false)
                override def send(cid: Long, replyTo: String, out: Array[Byte]): Unit = {
                  if (completed.compareAndSet(false, true))
                    writeResponse(ctx)(
                      WireFrame.Frame(1, FrameType.SYSTEM_EVENT, 0, requestCid, 0, 8, requestPath, "", out)
                    )
                }
              }
              port ! AdminPort.CmdIn(requestCid, requestPath, payload, sink)
            case None =>
              writeResponse(ctx)(
                WireFrame.Frame(
                  1, FrameType.SYSTEM_EVENT, 0, msg.correlationId, 0, 8, msg.path, "",
                  AdminV2Codec.encodeReply(
                    AdminV2Codec.AdminReplyV2.Failed(
                      msg.correlationId, AdminV2Executor.ErrDialectMismatch, "admin port disabled"
                    )
                  )
                )
              )
          }
        } // 非 admin tag：吞帧（前向兼容）
      case FrameType.ROUTE_HINT =>
      // 方案 A：hub 注入直连地址。JVM 客户端形态暂不建直连（出站经
      // akka selection 路由——直连优化属 Rust/erl/py spoke 侧）；吞帧
      // 不断连（协议前向兼容：未知帧类型才断）。
      case _ => // 握手重复/未知——断连
        WireConnection.close(ctx)
    }
  }

  /** Frame construction may fail before Netty sees a write (for example a
    * user-provided error code or Unicode detail). Keep that failure owned by
    * this connection, including callbacks running outside the event loop. */
  private def writeResponse(ctx: ChannelHandlerContext)(frame: => WireFrame.Frame): Unit = {
    if (!WireConnection.isTerminal(ctx)) {
      try {
        ctx.writeAndFlush(frame).addListener { (result: io.netty.channel.ChannelFuture) =>
          if (!result.isSuccess) WireConnection.close(ctx)
        }
      } catch {
        case scala.util.control.NonFatal(_) => WireConnection.close(ctx)
      }
    }
  }

  /** 半开检测：IdleStateHandler 10s 无入帧 → 关连接（registerTo 的
    * closeFuture 监听触发退避重拨）。hub 侧心跳 2s——10s 静默 = 死链。
    */
  override def userEventTriggered(ctx: ChannelHandlerContext, evt: java.lang.Object): Unit = {
    evt match {
      case idle: io.netty.handler.timeout.IdleStateEvent if idle.state() == io.netty.handler.timeout.IdleState.READER_IDLE =>
        System.err.println(s"[parrot-jvm] parrot silent >10s — half-open, closing")
        WireConnection.close(ctx)
      case _ => super.userEventTriggered(ctx, evt)
    }
  }

  override def exceptionCaught(ctx: ChannelHandlerContext, cause: Throwable): Unit =
    WireConnection.close(ctx)
}
object ParrotServerHandler {
  private[jvm] def actorPath(wirePath: String): String = {
    val local = if (wirePath.startsWith("parrot://")) {
      val authority = wirePath.stripPrefix("parrot://")
      val slash = authority.indexOf('/')
      if (slash < 0) "" else authority.substring(slash)
    } else wirePath
    if (local.startsWith("/jvm/user/")) local.stripPrefix("/jvm/user/")
    else if (local.startsWith("/user/")) local.stripPrefix("/user/")
    else if (local == "/jvm/user" || local == "/user") ""
    else local
  }

  /** Compatibility for existing transport bootstrap code. Each transport now
    * supplies its owner directly; no process-global ActorSystem is retained.
    * Direct business handlers receive their ActorSystem or scheduler plus owned budget.
    */
  @deprecated("Transport uses its constructor ActorSystem; remove this call", "0.1.0")
  def initSystem(sys: ActorSystem[_]): Unit = ()
  def askTimeout: akka.util.Timeout = akka.util.Timeout(6.seconds)
}

/** 注册模式 handler：连接建立即发 HANDSHAKE（客户端侧），收到 ACK 后
  * 进入与 server 相同的帧分发（复用 server 的握手后分支）。
  */
final class ParrotClientHandler(
    bridge: ActorRef[BridgeMsg],
    nodeId: String,
    adminPort: Option[ActorRef[AdminPort.Msg]] = None,
    scheduler: akka.actor.typed.Scheduler = null,
    askBudget: GatewayAskBudget = new GatewayAskBudget(),
    traceContext: Boolean = false
) extends ParrotServerHandler(bridge, nodeId, adminPort, scheduler, askBudget, traceContext) {
  def this(bridge: ActorRef[BridgeMsg], nodeId: String, adminPort: Option[ActorRef[AdminPort.Msg]],
           scheduler: akka.actor.typed.Scheduler, askBudget: GatewayAskBudget) =
    this(bridge, nodeId, adminPort, scheduler, askBudget, false)

  def this(bridge: ActorRef[BridgeMsg], nodeId: String, adminPort: Option[ActorRef[AdminPort.Msg]], system: ActorSystem[_]) = {
    this(bridge, nodeId, adminPort, system.scheduler, GatewayAskBudget.owned(system))
    ownAskBudget()
  }

  private var clientHandshaken = false

  override def channelActive(ctx: ChannelHandlerContext): Unit = {
    super.channelActive(ctx)
    // 客户端侧握手：主动发 HANDSHAKE，等对端 ACK（channelRead 覆盖分支处理）
    ctx.writeAndFlush(
      WireFrame.Frame(1, WireFrame.FrameType.HANDSHAKE, 0, 1, 0, 8, "", "__handshake__",
        WireFrame.handshakeBody(nodeId, gatewayCapabilities))
    ).addListener { (result: io.netty.channel.ChannelFuture) => if (!result.isSuccess) WireConnection.close(ctx); () }
  }

  override protected def handleFrame(ctx: ChannelHandlerContext, msg: WireFrame.Frame): Unit = {
    import WireFrame.FrameType
    if (!clientHandshaken) {
      msg.frameType match {
        case FrameType.HANDSHAKE_ACK =>
          val peer = WireFrame.decodeHandshake(msg.payload, acknowledgement = true)
          negotiatePeer(ctx, peer)
          completeHandshake(ctx)
          clientHandshaken = handshaken
          System.out.println(s"PARROT_JVM_REGISTERED=$nodeId")
          System.out.flush()
        case _ => WireConnection.close(ctx) // 未完成握手先数据 = 协议违规
      }
      return
    }
    super.handleFrame(ctx, msg) // 握手后：复用 server 分发
  }

}
