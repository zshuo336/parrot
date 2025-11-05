package parrot.protocol.jvm

import io.netty.channel.{Channel, ChannelHandlerContext}
import io.netty.util.AttributeKey

/** Sticky per-connection admission fence. close() can be asynchronous, and
  * ByteToMessageDecoder can still deliver decoded frames or call decodeLast.
  */
private[jvm] object WireConnection {
  private val Terminal = AttributeKey.valueOf[java.lang.Boolean]("parrot.jvm.terminal")
  def mark(channel: Channel): Unit = channel.attr(Terminal).set(java.lang.Boolean.TRUE)
  def isTerminal(ctx: ChannelHandlerContext): Boolean =
    !ctx.channel().isActive || ctx.channel().attr(Terminal).get() == java.lang.Boolean.TRUE
  def close(ctx: ChannelHandlerContext): Unit = {
    mark(ctx.channel())
    ctx.close()
    ()
  }
}
