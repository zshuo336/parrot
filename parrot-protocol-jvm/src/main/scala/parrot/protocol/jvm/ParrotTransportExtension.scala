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
    nodeId: String
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
            .addLast(new ParrotServerHandler(bridge, nodeId))
        }
      })
    val f = b.bind(port0).sync()
    boundPort = f.channel().localAddress().asInstanceOf[java.net.InetSocketAddress].getPort
  }

  def shutdown(): Unit = if (group != null) group.shutdownGracefully()
}

/** 半包解码器：复刻 Frame::decode 语义（不足一帧不消费 readerIndex）。 */
final class WireDecoder extends ByteToMessageDecoder {
  override def decode(ctx: ChannelHandlerContext, in: ByteBuf, out: java.util.List[Object]): Unit = {
    if (in.readableBytes() < 4) return
    val bodyLen = in.getIntLE(in.readerIndex())
    if (bodyLen < 0 || bodyLen > WireFrame.MAX_FRAME_LEN) {
      ctx.close()
      return
    }
    if (in.readableBytes() < 4 + bodyLen) return // 半包：不消费
    val buf = new Array[Byte](4 + bodyLen)
    in.readBytes(buf)
    WireFrame.decode(buf) match {
      case Some((frame, _)) => out.add(frame)
      case None             => ctx.close() // 坏帧断连
    }
  }
}

final class WireEncoder extends io.netty.handler.codec.MessageToByteEncoder[WireFrame.Frame] {
  override def encode(ctx: ChannelHandlerContext, msg: WireFrame.Frame, out: ByteBuf): Unit =
    out.writeBytes(msg.encode())
}

final class ParrotServerHandler(bridge: ActorRef[BridgeMsg], nodeId: String)
    extends SimpleChannelInboundHandler[WireFrame.Frame] {
  private var handshaken = false

  override def channelRead0(ctx: ChannelHandlerContext, msg: WireFrame.Frame): Unit = {
    import WireFrame.FrameType
    if (!handshaken) {
      msg.frameType match {
        case FrameType.HANDSHAKE =>
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
            ctx.writeAndFlush(WireFrame.Frame(1, FrameType.REPLY, 0, cid, 0, 8, replyTo, key, payload))
          case Success(ReplyErr(cid, code, detail)) =>
            ctx.writeAndFlush(
              WireFrame.Frame(1, FrameType.REPLY_ERR, 0, cid, 0, 8, replyTo, "", WireFrame.encodeErrPayload(code, detail))
            )
          case Failure(ex) =>
            ctx.writeAndFlush(
              WireFrame.Frame(
                1, FrameType.REPLY_ERR, 0, msg.correlationId, 0, 8, replyTo, "",
                WireFrame.encodeErrPayload(WireFrame.ErrCode.Timeout, Option(ex.getMessage).getOrElse("ask failed"))
              )
            )
        }(scala.concurrent.ExecutionContext.parasitic)
      case FrameType.TELL =>
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
        ctx.writeAndFlush(WireFrame.Frame(1, FrameType.HEARTBEAT_ACK, 0, 0, 0, 8, "", "", Array.emptyByteArray))
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
