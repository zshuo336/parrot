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
    adminPort: Option[ActorRef[AdminPort.Msg]] = None
) {
  private var group: NioEventLoopGroup = _
  @volatile private var boundPort: Int = -1

  def port: Int = boundPort

  def listen(port0: Int): Unit = {
    group = new NioEventLoopGroup(1, new DefaultThreadFactory("parrot-jvm-netty"))
    val b = new ServerBootstrap()
      .group(group)
      .channel(classOf[NioServerSocketChannel])
      .childHandler(new ChannelInitializer[SocketChannel] {
        override def initChannel(ch: SocketChannel): Unit = {
            ch.pipeline()
            .addLast(new WireDecoder())
            .addLast(new WireEncoder())
            .addLast(new ParrotServerHandler(bridge, nodeId, adminPort))
        }
      })
    val f = b.bind(port0).sync()
    boundPort = f.channel().localAddress().asInstanceOf[java.net.InetSocketAddress].getPort
  }

  /** 注册模式（双模式组网 + 容灾重连）：主动拨号 parrot 节点并发起客户端握手
    * （发 HANDSHAKE → 收 ACK），随后与被动模式同一 handler 服务。
    * 断线（channelInactive）→ scheduler 指数退避重拨（1s→60s 封顶）——
    * hub 重启/网络抖动后 JVM 网关自动重新接入，不再成为孤儿。
    */
  def registerTo(parrotHost: String, parrotPort: Int): Unit = {
    group = new NioEventLoopGroup(1, new DefaultThreadFactory("parrot-jvm-netty"))
    val scheduler = system.scheduler
    import system.executionContext
    @volatile var attempt = 0

    def dial(): Unit = {
      val b = new io.netty.bootstrap.Bootstrap()
        .group(group)
        .channel(classOf[io.netty.channel.socket.nio.NioSocketChannel])
        .handler(new io.netty.channel.ChannelInitializer[io.netty.channel.socket.SocketChannel] {
          override def initChannel(ch: io.netty.channel.socket.SocketChannel): Unit = {
            ch.pipeline()
              // 半开检测：10s 无入帧判 hub 死亡 → 关连接 → inactive → 重拨
              .addLast(new io.netty.handler.timeout.IdleStateHandler(10, 0, 0))
              .addLast(new WireDecoder())
              .addLast(new WireEncoder())
              .addLast(new ParrotClientHandler(bridge, nodeId, adminPort))
          }
        })
      val f = b.connect(parrotHost, parrotPort)
      f.addListener { (future: io.netty.channel.ChannelFuture) =>
        if (future.isSuccess) {
          attempt = 0 // 成功即重置退避
        } else {
          val delay = math.min(60, math.pow(2, math.min(attempt, 6))).seconds
          attempt += 1
          System.err.println(s"[parrot-jvm] connect parrot failed (${future.cause()}); retry in $delay")
          scheduler.scheduleOnce(java.time.Duration.ofSeconds(delay.toSeconds), () => dial(), system.executionContext)
        }
      }
      f.channel().closeFuture().addListener { (_: io.netty.channel.ChannelFuture) =>
        val delay = math.min(60, math.pow(2, math.min(attempt, 6))).seconds
        attempt += 1
        System.err.println(s"[parrot-jvm] parrot link lost; reconnecting in $delay")
        scheduler.scheduleOnce(java.time.Duration.ofSeconds(delay.toSeconds), () => dial(), system.executionContext)
      }
    }
    dial()
  }

  def shutdown(): Unit = if (group != null) group.shutdownGracefully()
}

/** 半包解码器：直接在 ByteBuf 上解析（DEV_08 零拷贝——跳过旧实现
  * readBytes→Array→WireFrame.decode 的中转拷贝）。语义复刻 Frame::decode：
  * 不足一帧不推进 readerIndex。
  */
final class WireDecoder extends ByteToMessageDecoder {
  override def decode(ctx: ChannelHandlerContext, in: ByteBuf, out: java.util.List[Object]): Unit = {
    if (in.readableBytes() < 4) return
    val bodyLen = in.getIntLE(in.readerIndex())
    if (bodyLen < 0 || bodyLen > WireFrame.MAX_FRAME_LEN) {
      ctx.close()
      return
    }
    if (in.readableBytes() < 4 + bodyLen) return // 半包：不消费
    val frameStart = in.readerIndex()
    in.skipBytes(4) // body_len 前缀
    val version = in.readByte()
    if (version != WireFrame.PROTOCOL_VERSION) {
      in.readerIndex(frameStart)
      ctx.close() // 坏版本断连
      return
    }
    val ft     = in.readByte()
    val flags  = in.readShortLE() & 0xFFFF
    val cid    = in.readLongLE()
    val hopCnt = in.readByte()
    val hopLmt = in.readByte()
    in.skipBytes(6) // reserved u48
    val pathLen = in.readIntLE()
    val path    = in.readCharSequence(pathLen, java.nio.charset.StandardCharsets.UTF_8).toString
    val keyLen  = in.readIntLE()
    val key     = in.readCharSequence(keyLen, java.nio.charset.StandardCharsets.UTF_8).toString
    val payloadLen = bodyLen - WireFrame.BODY_FIXED_OVERHEAD - pathLen - keyLen
    if (payloadLen < 0 || in.readableBytes() < payloadLen) {
      in.readerIndex(frameStart) // 长度域不一致——保守按半包处理
      return
    }
    // payload 直接 retain 切片（引用计数随帧传递；encoder 消费后 release）
    val payload = in.readRetainedSlice(payloadLen)
    val f = new WireFrame.Frame(version, ft, flags, cid, hopCnt, hopLmt, path, key, payload)
    out.add(f)
  }
}

/** 零拷贝编码：直接写 ByteBuf（跳过 encode() 数组中转）。 */
final class WireEncoder extends io.netty.handler.codec.MessageToByteEncoder[WireFrame.Frame] {
  override def encode(ctx: ChannelHandlerContext, msg: WireFrame.Frame, out: ByteBuf): Unit = {
    val pathB = msg.path.getBytes(java.nio.charset.StandardCharsets.UTF_8)
    val keyB  = msg.typeKey.getBytes(java.nio.charset.StandardCharsets.UTF_8)
    val payload = msg.payloadBytes
    val body = WireFrame.BODY_FIXED_OVERHEAD + pathB.length + keyB.length + payload.readableBytes()
    out.ensureWritable(4 + body)
    out.writeIntLE(body)
    out.writeByte(msg.version)
    out.writeByte(msg.frameType)
    out.writeShortLE(msg.flags)
    out.writeLongLE(msg.correlationId)
    out.writeByte(msg.hopCount)
    out.writeByte(msg.hopLimit)
    out.writeZero(6) // reserved u48
    out.writeIntLE(pathB.length)
    out.writeBytes(pathB)
    out.writeIntLE(keyB.length)
    out.writeBytes(keyB)
    out.writeBytes(payload) // ByteBuf 直写（不落堆数组）
    msg.releaseBuf() // retained 切片生命周期到此（数组形态 no-op）
  }
}

class ParrotServerHandler(
    bridge: ActorRef[BridgeMsg],
    nodeId: String,
    adminPort: Option[ActorRef[AdminPort.Msg]] = None
)
    extends SimpleChannelInboundHandler[WireFrame.Frame] {
  /** 子类（注册模式 client handler）可见——客户端握手完成后置位复用分发。 */

  override def channelActive(ctx: ChannelHandlerContext): Unit = {
    GatewayMetrics.counters.connections.incrementAndGet()
    super.channelActive(ctx)
  }

  override def channelInactive(ctx: ChannelHandlerContext): Unit = {
    GatewayMetrics.counters.connections.decrementAndGet()
    super.channelInactive(ctx)
  }
  protected var handshaken = false

  override def channelRead0(ctx: ChannelHandlerContext, msg: WireFrame.Frame): Unit = {
    import WireFrame.FrameType
    import GatewayMetrics.counters
    counters.bytesRx.addAndGet(msg.payload.length.toLong)
    if (!handshaken) {
      msg.frameType match {
        case FrameType.HANDSHAKE =>
          counters.handshakesOk.incrementAndGet()
          ctx.writeAndFlush(
            WireFrame.Frame(1, FrameType.HANDSHAKE_ACK, 0, 0, 0, 8, "", "", WireFrame.handshakeAckBody(nodeId))
          )
          handshaken = true
        case _ => ctx.close() // 未握手先数据 = 协议违规
      }
      return
    }
    msg.frameType match {
      case FrameType.ASK =>
        counters.asksRx.incrementAndGet()
        // path（两种形态）：parrot://{gw}/jvm/user/{akkaPath} 或 /jvm/user/{akkaPath}
        val p0    = msg.path.stripPrefix("parrot://")
        val after = if (p0 != msg.path) { // 有前缀：剥节点段
          val idx = p0.indexOf('/')
          if (idx >= 0) p0.substring(idx) else p0
        } else msg.path
        val akkaPath = after
          .stripPrefix("/jvm/user/")
          .stripPrefix("/jvm/user")
        val (replyTo, realPayload) = WireFrame.splitReplyTo(msg.payload)
          .getOrElse(("", msg.payload))
        implicit val timeout: akka.util.Timeout = akka.util.Timeout(6.seconds)
        implicit val sch = io.netty.util.concurrent.GlobalEventExecutor.INSTANCE // 不用于 ask——
        // ask 走 actor system scheduler：
        import ParrotServerHandler._
        val fut = bridge.ask[BridgeReply](
          ref => Ask(msg.correlationId, akkaPath, msg.typeKey, realPayload, ref)
        )(askTimeout, askScheduler)
        fut.onComplete {
          case Success(Replied(cid, key, payload)) =>
            counters.repliesTx.incrementAndGet()
            counters.bytesTx.addAndGet(payload.length.toLong)
            ctx.writeAndFlush(WireFrame.Frame(1, FrameType.REPLY, 0, cid, 0, 8, replyTo, key, payload))
          case Success(ReplyErr(cid, code, detail)) =>
            counters.repliesTx.incrementAndGet()
            counters.replyErrs.incrementAndGet()
            ctx.writeAndFlush(
              WireFrame.Frame(1, FrameType.REPLY_ERR, 0, cid, 0, 8, replyTo, "", WireFrame.encodeErrPayload(code, detail))
            )
          case Failure(ex) =>
            counters.repliesTx.incrementAndGet()
            counters.replyErrs.incrementAndGet()
            ctx.writeAndFlush(
              WireFrame.Frame(
                1, FrameType.REPLY_ERR, 0, msg.correlationId, 0, 8, replyTo, "",
                WireFrame.encodeErrPayload(WireFrame.ErrCode.Timeout, Option(ex.getMessage).getOrElse("ask failed"))
              )
            )
        }(scala.concurrent.ExecutionContext.parasitic)
      case FrameType.TELL =>
        counters.tellsRx.incrementAndGet()
        val p0    = msg.path.stripPrefix("parrot://")
        val after = if (p0 != msg.path) {
          val idx = p0.indexOf('/')
          if (idx >= 0) p0.substring(idx) else p0
        } else msg.path
        val akkaPath = after
          .stripPrefix("/jvm/user/")
          .stripPrefix("/jvm/user")
        bridge ! Tell(akkaPath, msg.typeKey, msg.payload)
      case FrameType.STOP =>
        ctx.writeAndFlush(
          WireFrame.Frame(
            1, FrameType.ERROR, 0, msg.correlationId, 0, 8, "", "",
            WireFrame.encodeErrPayload(WireFrame.ErrCode.Forbidden, "akka stop unsupported (I4)")
          )
        )
      case FrameType.HEARTBEAT =>
        counters.heartbeatsRx.incrementAndGet()
        ctx.writeAndFlush(WireFrame.Frame(1, FrameType.HEARTBEAT_ACK, 0, 0, 0, 8, "", "", Array.emptyByteArray))
      case FrameType.SYSTEM_EVENT =>
        // B5（DEV_09）：admin-v2（tag 0x03）→ AdminPort 执行回 0x04 回执帧；
        // 其它 tag（gossip 等）吞帧不断连（与 ray/erl 方言一致）。
        val payload = msg.payload
        if (payload.nonEmpty && (payload(0) & 0xFF) == AdminV2Codec.TagAdminCmdV2) {
          adminPort match {
            case Some(port) =>
              val sink = new AdminPort.ReplySink {
                override def send(cid: Long, replyTo: String, out: Array[Byte]): Unit =
                  // 回执帧：type_key 留空；path 回写发起方 reply_to
                  ctx.writeAndFlush(
                    WireFrame.Frame(1, FrameType.SYSTEM_EVENT, 0, cid, 0, 8, replyTo, "", out)
                  )
              }
              port ! AdminPort.CmdIn(msg.correlationId, msg.path, payload, sink)
            case None =>
              ctx.writeAndFlush(
                WireFrame.Frame(
                  1, FrameType.SYSTEM_EVENT, 0, msg.correlationId, 0, 8, "", "",
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
        ctx.close()
    }
  }

  override def exceptionCaught(ctx: ChannelHandlerContext, cause: Throwable): Unit =
    ctx.close()
}
object ParrotServerHandler {
  @volatile private var _system: Option[ActorSystem[_]] = None
  def initSystem(sys: ActorSystem[_]): Unit = _system = Some(sys)
  def askTimeout: akka.util.Timeout = akka.util.Timeout(6.seconds)
  def askScheduler: akka.actor.typed.Scheduler =
    _system.map(_.scheduler).getOrElse(
      throw new IllegalStateException("initSystem not called")
    )
}

/** 注册模式 handler：连接建立即发 HANDSHAKE（客户端侧），收到 ACK 后
  * 进入与 server 相同的帧分发（复用 server 的握手后分支）。
  */
final class ParrotClientHandler(
    bridge: ActorRef[BridgeMsg],
    nodeId: String,
    adminPort: Option[ActorRef[AdminPort.Msg]] = None
) extends ParrotServerHandler(bridge, nodeId, adminPort) {

  private var clientHandshaken = false

  override def channelActive(ctx: ChannelHandlerContext): Unit = {
    // 客户端侧握手：主动发 HANDSHAKE，等对端 ACK（channelRead 覆盖分支处理）
    ctx.writeAndFlush(
      WireFrame.Frame(1, WireFrame.FrameType.HANDSHAKE, 0, 1, 0, 8, "", "__handshake__",
        WireFrame.handshakeBody(nodeId))
    )
  }

  override def channelRead0(ctx: ChannelHandlerContext, msg: WireFrame.Frame): Unit = {
    import WireFrame.FrameType
    if (!clientHandshaken) {
      msg.frameType match {
        case FrameType.HANDSHAKE_ACK =>
          clientHandshaken = true
          handshaken = true // 父类分发放行（同一连接已完成握手语义）
          System.out.println(s"PARROT_JVM_REGISTERED=$nodeId")
          System.out.flush()
        case _ => ctx.close() // 未完成握手先数据 = 协议违规
      }
      return
    }
    super.channelRead0(ctx, msg) // 握手后：复用 server 分发
  }

  /** 半开检测：IdleStateHandler 10s 无入帧 → 关连接（registerTo 的
    * closeFuture 监听触发退避重拨）。hub 侧心跳 2s——10s 静默 = 死链。
    */
  override def userEventTriggered(ctx: ChannelHandlerContext, evt: java.lang.Object): Unit = {
    evt match {
      case idle: io.netty.handler.timeout.IdleStateEvent if idle.state() == io.netty.handler.timeout.IdleState.READER_IDLE =>
        System.err.println(s"[parrot-jvm] parrot silent >10s — half-open, closing")
        ctx.close()
      case _ => super.userEventTriggered(ctx, evt)
    }
  }
}
