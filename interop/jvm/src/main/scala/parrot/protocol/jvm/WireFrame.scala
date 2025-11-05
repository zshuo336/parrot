package parrot.protocol.jvm

import java.nio.charset.StandardCharsets.UTF_8
import java.nio.charset.{CharacterCodingException, CodingErrorAction}
import io.netty.buffer.{ByteBuf, Unpooled}

/** Wire 1.0 帧（与 parrot-remote frame.rs 字节级一致）。
  *
  * 布局：[u32 frame_len][u8 version][u8 frame_type][u16 flags][u64 cid]
  *       [u8 hop_count][u8 hop_limit][u32 sequence][u16 reserved=0][u32 path_len][path]
  *       [u32 key_len][key][payload]；全部 LE。
  *
  * frame_len = 28B 定长 + path/key/payload（不含自身 4B 前缀）。
  */
object WireFrame {
  val PROTOCOL_VERSION: Byte = 0x01
  val MAX_FRAME_LEN = 16 * 1024 * 1024
  val BODY_FIXED_OVERHEAD = 28 // includes both path_len and key_len

  object FrameType {
    val HANDSHAKE: Byte = 0x01
    val HANDSHAKE_ACK: Byte = 0x02
    val HEARTBEAT: Byte = 0x03
    val HEARTBEAT_ACK: Byte = 0x04
    val ASK: Byte = 0x10
    val REPLY: Byte = 0x11
    val REPLY_ERR: Byte = 0x12
    val TELL: Byte = 0x13
    val STOP: Byte = 0x14
    val FRAGMENT: Byte = 0x15
    val SYSTEM_EVENT: Byte = 0x20
    val ROUTE_HINT: Byte = 0x24
    val ERROR: Byte = 0x7F
  }

  /** payload 双形态（DEV_08 零拷贝）：
    * - Netty 路径：retainedBuf（readRetainedSlice 直传，由接收 handler 或发送 encoder release）；
    * - 纯编解码/单测：bytes（lazy 物化）。
    * 二者互斥：构造时二选一，另一侧首次访问时物化（Netty 切片→数组仅
    * 在确实需要数组形态时发生——热路径零拷贝）。
    */
  final class Frame(
      val version: Byte,
      val frameType: Byte,
      val flags: Int,
      val correlationId: Long,
      val hopCount: Byte,
      val hopLimit: Byte,
      val path: String,
      val typeKey: String,
      bytes0: Array[Byte],
      buf0: io.netty.buffer.ByteBuf,
      val sequence: Long = 0L
  ) {
    private var _bytes: Array[Byte] = bytes0
    private var _buf: io.netty.buffer.ByteBuf = buf0

    def this(version: Byte, frameType: Byte, flags: Int, correlationId: Long,
             hopCount: Byte, hopLimit: Byte, path: String, typeKey: String,
             payload: Array[Byte]) =
      this(version, frameType, flags, correlationId, hopCount, hopLimit, path, typeKey, payload, null)

    private[jvm] def this(version: Byte, frameType: Byte, flags: Int, correlationId: Long,
                          hopCount: Byte, hopLimit: Byte, path: String, typeKey: String,
                          payloadBuf: io.netty.buffer.ByteBuf) =
      this(version, frameType, flags, correlationId, hopCount, hopLimit, path, typeKey, null, payloadBuf)

    /** 数组形态（需拷贝时一次性物化）。 */
    def payload: Array[Byte] = {
      if (_bytes == null) {
        val b = new Array[Byte](_buf.readableBytes())
        val dup = _buf.duplicate() // 不动读指针
        dup.readBytes(b)
        _bytes = b
      }
      _bytes
    }

    private[jvm] def payloadLength: Int =
      if (_buf != null) _buf.readableBytes() else _bytes.length

    private[jvm] def payloadPrefix(length: Int): Array[Byte] = {
      require(length >= 0 && length <= payloadLength, "truncated payload prefix")
      val prefix = new Array[Byte](length)
      if (_buf != null) _buf.getBytes(_buf.readerIndex(), prefix)
      else System.arraycopy(_bytes, 0, prefix, 0, length)
      prefix
    }

    private[jvm] def writePayload(out: ByteBuf): Unit =
      if (_buf != null) out.writeBytes(_buf, _buf.readerIndex(), _buf.readableBytes())
      else out.writeBytes(_bytes)

    /** 消费掉 retained 资源（encoder 写完调用；幂等）。 */
    private[jvm] def releaseBuf(): Unit = {
      if (_buf != null && _buf.refCnt() > 0) _buf.release()
      _buf = null
    }

    def encode(): Array[Byte] = {
      val out = Unpooled.buffer()
      try {
        writeFrame(this, out)
        val bytes = new Array[Byte](out.readableBytes())
        out.readBytes(bytes)
        bytes
      } finally out.release()
    }

    override def equals(that: Any): Boolean = that match {
      case f: Frame =>
        version == f.version && frameType == f.frameType && flags == f.flags &&
          correlationId == f.correlationId && hopCount == f.hopCount && hopLimit == f.hopLimit &&
          path == f.path && typeKey == f.typeKey && sequence == f.sequence &&
          java.util.Arrays.equals(payload, f.payload)
      case _ => false
    }

    override def hashCode(): Int =
      java.util.Objects.hash(
        Byte.box(version), Byte.box(frameType), Int.box(flags), Long.box(correlationId),
        Byte.box(hopCount), Byte.box(hopLimit), path, typeKey, Long.box(sequence), java.util.Arrays.hashCode(payload)
      )

    override def toString: String =
      s"Frame($version, $frameType, $flags, $correlationId, $hopCount, $hopLimit, $path, $typeKey, payload[${payload.length}])"
  }

  object Frame {
    /** Array 形态工厂（旧 case class 构造兼容点）。 */
    def apply(version: Byte, frameType: Byte, flags: Int, correlationId: Long,
              hopCount: Byte, hopLimit: Byte, path: String, typeKey: String,
              payload: Array[Byte]): Frame =
      new Frame(version, frameType, flags, correlationId, hopCount, hopLimit, path, typeKey, payload)

    /** ByteBuf 形态工厂（Netty 零拷贝路径——retained 切片归本帧）。 */
    private[jvm] def apply(version: Byte, frameType: Byte, flags: Int, correlationId: Long,
                           hopCount: Byte, hopLimit: Byte, path: String, typeKey: String,
                           payloadBuf: io.netty.buffer.ByteBuf): Frame =
      new Frame(version, frameType, flags, correlationId, hopCount, hopLimit, path, typeKey, payloadBuf)
  }

  private def readU32(b: Array[Byte], off: Int): Long =
    (b(off).toLong & 0xFF) | ((b(off + 1).toLong & 0xFF) << 8) |
      ((b(off + 2).toLong & 0xFF) << 16) | ((b(off + 3).toLong & 0xFF) << 24)

  private def writeU32(b: Array[Byte], off: Int, v: Long): Unit = {
    b(off) = (v & 0xFF).toByte
    b(off + 1) = ((v >> 8) & 0xFF).toByte
    b(off + 2) = ((v >> 16) & 0xFF).toByte
    b(off + 3) = ((v >> 24) & 0xFF).toByte
  }

  private val knownTypes = Set[Byte](1, 2, 3, 4, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15,
    0x20, 0x21, 0x22, 0x23, 0x24, 0x7f)

  private def validateHeader(version: Byte, ft: Byte, hop: Byte, limit: Byte): Unit = {
    require(version == PROTOCOL_VERSION, "unsupported wire version")
    require(knownTypes.contains(ft), "unknown frame type")
    require((limit & 0xff) > 0 && (hop & 0xff) < (limit & 0xff), "invalid hop bounds")
  }

  private def strictUtf8(bytes: java.nio.ByteBuffer): String =
    try UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT)
      .onUnmappableCharacter(CodingErrorAction.REPORT).decode(bytes).toString
    catch { case e: CharacterCodingException => throw new IllegalArgumentException("invalid UTF-8", e) }

  private def utf8Bytes(value: String): Array[Byte] = {
    val encoded = try UTF_8.newEncoder().onMalformedInput(CodingErrorAction.REPORT)
      .onUnmappableCharacter(CodingErrorAction.REPORT).encode(java.nio.CharBuffer.wrap(value))
    catch { case e: CharacterCodingException => throw new IllegalArgumentException("invalid Unicode", e) }
    val bytes = new Array[Byte](encoded.remaining())
    encoded.get(bytes)
    bytes
  }

  /** Shared layout validation. Absolute reads never consume a partial frame or borrow
    * bytes from the next frame. Only a fully validated payload is retained. */
  private[jvm] def decodeBuffer(in: ByteBuf): Option[(Frame, Int)] = {
    if (in.readableBytes() < 4) return None
    val start = in.readerIndex()
    val bodyLen = in.getUnsignedIntLE(start)
    require(bodyLen >= BODY_FIXED_OVERHEAD && bodyLen <= MAX_FRAME_LEN, "invalid frame length")
    val total = 4 + bodyLen.toInt
    if (in.readableBytes() < total) return None
    val version = in.getByte(start + 4)
    val ft = in.getByte(start + 5)
    val hop = in.getByte(start + 16)
    val limit = in.getByte(start + 17)
    validateHeader(version, ft, hop, limit)
    require(in.getUnsignedShortLE(start + 22) == 0, "invalid reserved bits")
    val pathLen = in.getUnsignedIntLE(start + 24)
    require(pathLen <= bodyLen - BODY_FIXED_OVERHEAD, "path exceeds frame")
    val keyOffset = start + 28 + pathLen.toInt
    val keyLen = in.getUnsignedIntLE(keyOffset)
    require(keyLen <= bodyLen - BODY_FIXED_OVERHEAD - pathLen, "key exceeds frame")
    val path = strictUtf8(in.nioBuffer(start + 28, pathLen.toInt))
    val key = strictUtf8(in.nioBuffer(keyOffset + 4, keyLen.toInt))
    val payloadOffset = keyOffset + 4 + keyLen.toInt
    val payloadLen = total - (payloadOffset - start)
    val frame = new Frame(version, ft, in.getUnsignedShortLE(start + 6), in.getLongLE(start + 8),
      hop, limit, path, key, null, in.retainedSlice(payloadOffset, payloadLen),
      in.getUnsignedIntLE(start + 18))
    Some((frame, total))
  }

  def decode(bytes: Array[Byte]): Option[(Frame, Int)] = {
    val in = Unpooled.wrappedBuffer(bytes)
    try decodeBuffer(in).map { case (frame, consumed) =>
      try (new Frame(frame.version, frame.frameType, frame.flags, frame.correlationId,
        frame.hopCount, frame.hopLimit, frame.path, frame.typeKey, frame.payload, null, frame.sequence), consumed)
      finally frame.releaseBuf()
    } finally in.release()
  }

  /** Both encoders validate the same contract and preserve the sequence extension.
    * The caller owns frame retirement; copying does not advance payload indices. */
  private[jvm] def writeFrame(frame: Frame, out: ByteBuf): Unit = {
    validateHeader(frame.version, frame.frameType, frame.hopCount, frame.hopLimit)
    require(frame.flags >= 0 && frame.flags <= 0xffff, "invalid flags")
    require(frame.sequence >= 0 && frame.sequence <= 0xffffffffL, "invalid sequence")
    val path = utf8Bytes(frame.path)
    val key = utf8Bytes(frame.typeKey)
    val body = BODY_FIXED_OVERHEAD.toLong + path.length + key.length + frame.payloadLength
    require(body <= MAX_FRAME_LEN, "frame exceeds maximum")
    out.ensureWritable(4 + body.toInt)
    out.writeIntLE(body.toInt)
    out.writeByte(frame.version)
    out.writeByte(frame.frameType)
    out.writeShortLE(frame.flags)
    out.writeLongLE(frame.correlationId)
    out.writeByte(frame.hopCount)
    out.writeByte(frame.hopLimit)
    out.writeIntLE(frame.sequence.toInt)
    out.writeShortLE(0)
    out.writeIntLE(path.length)
    out.writeBytes(path)
    out.writeIntLE(key.length)
    out.writeBytes(key)
    frame.writePayload(out)
  }

  // ---------------- 错误体（与 error.rs encode_err_payload 一致） ----------------

  object ErrCode {
    val ActorNotFound    = 1
    val Timeout          = 2
    val Stopped          = 3
    val NotRemotable     = 4
    val CodecError       = 5
    val UnknownTypeKey   = 6
    val RouteUnreachable = 7
    val ConnectionLost   = 8
    val DirectoryStale   = 9
    val Overloaded       = 10
    val NoCommonCodec    = 11
    val ProtocolViolation = 12
    val Forbidden        = 13
  }

  /** REPLY_ERR/ERROR payload：[u16 code][u16 rsv=0][detail utf8]。 */
  def encodeErrPayload(code: Int, detail: String): Array[Byte] = {
    require(code >= 1 && code <= 13, "unknown error code")
    val d = utf8Bytes(detail)
    val out = new Array[Byte](4 + d.length)
    out(0) = (code & 0xFF).toByte
    out(1) = ((code >> 8) & 0xFF).toByte
    out(2) = 0
    out(3) = 0
    System.arraycopy(d, 0, out, 4, d.length)
    out
  }

  def decodeErrPayload(b: Array[Byte]): (Int, String) = {
    require(b.length >= 4 && b(2) == 0 && b(3) == 0, "invalid error envelope")
    val code = (b(0).toInt & 0xFF) | ((b(1).toInt & 0xFF) << 8)
    require(code >= 1 && code <= 13, "unknown error code")
    (code, strictUtf8(java.nio.ByteBuffer.wrap(b, 4, b.length - 4)))
  }

  // ---------------- ASK reply_to 前缀（与 frame.rs split_reply_to 一致） ----------------

  def withReplyToPrefix(replyTo: String, payload: Array[Byte]): Array[Byte] = {
    val rb = utf8Bytes(replyTo)
    val out = new Array[Byte](4 + rb.length + payload.length)
    writeU32(out, 0, rb.length)
    System.arraycopy(rb, 0, out, 4, rb.length)
    System.arraycopy(payload, 0, out, 4 + rb.length, payload.length)
    out
  }

  /** 剥 reply_to 前缀：返回 (replyTo, 真实 payload)。 */
  def splitReplyTo(b: Array[Byte]): Option[(String, Array[Byte])] =
    if (b.length < 4) None
    else {
      val length = readU32(b, 0)
      if (length > b.length - 4) None
      else {
        val rlen = length.toInt
        val rto = try strictUtf8(java.nio.ByteBuffer.wrap(b, 4, rlen))
        catch { case _: IllegalArgumentException => return None }
        Some((rto, java.util.Arrays.copyOfRange(b, 4 + rlen, b.length)))
      }
    }

  // ---------------- 握手 TLV（与 handshake.rs 同源布局：tag u8 + len u16 LE + value） ----------------

  object TlvTag {
    val NODE_ID: Byte = 1
    val REALM: Byte = 2
    val CLUSTER: Byte = 3
    /** u32 LE 位域：bit0 bincode / bit1 pb / bit2 zstd / bit3 quic / bit4 ws */
    val CAPABILITIES: Byte = 4
    val MAX_FRAME_LEN: Byte = 5
    /** u8：0 normal / 1 hub / 2 border / 3 directory */
    val TOPOLOGY_ROLE: Byte = 6
    val HOP_LIMIT: Byte = 7
    val DIRECT_ADDR: Byte = 8
    val CHOSEN_CODEC: Byte = 9
  }

  /** 能力位：JVM 网关只说 bincode 栈（bit0）。 */
  val CAPS_BIN_ONLY: Int = 0x01
  val CAPS_MEASURED: Int = 1 << 10
  val CAPS_TRACE_CONTEXT: Int = 1 << 13
  val FLAG_TRACE_CONTEXT: Int = 1 << 6
  private[jvm] val TraceContextNegotiated = io.netty.util.AttributeKey.valueOf[java.lang.Boolean]("parrot.trace-context-negotiated")

  def handshakeBody(nodeId: String, capabilities: Int = CAPS_BIN_ONLY): Array[Byte] = {
    val id = utf8Bytes(nodeId)
    require(id.nonEmpty && id.length <= 65535 && !nodeId.contains('\u0000'), "invalid node identity")
    val bb = java.nio.ByteBuffer
      .allocate(3 + id.length + 3 + 4 + 3 + 4 + 3 + 1 + 3 + 1)
      .order(java.nio.ByteOrder.LITTLE_ENDIAN)
    bb.put(TlvTag.NODE_ID); bb.putShort(id.length.toShort); bb.put(id)
    bb.put(TlvTag.CAPABILITIES); bb.putShort(4.toShort); bb.putInt(capabilities)
    bb.put(TlvTag.MAX_FRAME_LEN); bb.putShort(4.toShort); bb.putInt(MAX_FRAME_LEN)
    bb.put(TlvTag.TOPOLOGY_ROLE); bb.putShort(1.toShort); bb.put(0.toByte) // normal
    bb.put(TlvTag.HOP_LIMIT); bb.putShort(1.toShort); bb.put(8.toByte)
    bb.array()
  }

  /** ACK 体：同族字段 + chosen_codec=bin（1.0 固定 bin 优先）。 */
  def handshakeAckBody(nodeId: String, capabilities: Int = CAPS_BIN_ONLY): Array[Byte] = {
    require((capabilities & CAPS_BIN_ONLY) != 0, "chosen bin codec is not advertised")
    val base = handshakeBody(nodeId, capabilities)
    val cc   = "bin".getBytes(UTF_8)
    val bb = java.nio.ByteBuffer
      .allocate(base.length + 3 + cc.length)
      .order(java.nio.ByteOrder.LITTLE_ENDIAN)
    bb.put(base, 0, base.length)
    bb.put(TlvTag.CHOSEN_CODEC); bb.putShort(cc.length.toShort); bb.put(cc)
    bb.array()
  }

  def parseTlv(body: Array[Byte]): List[(Byte, Array[Byte])] = {
    val out = scala.collection.mutable.ListBuffer.empty[(Byte, Array[Byte])]
    var p = 0
    while (p < body.length) {
      require(body.length - p >= 3, "truncated TLV header")
      val tag = body(p)
      val len = (body(p + 1).toInt & 0xFF) | ((body(p + 2).toInt & 0xFF) << 8)
      require(len <= body.length - p - 3, "truncated TLV value")
      out += ((tag, java.util.Arrays.copyOfRange(body, p + 3, p + 3 + len)))
      p += 3 + len
    }
    out.toList
  }

  private[jvm] val HandshakeComplete = io.netty.util.AttributeKey.valueOf[java.lang.Boolean]("parrot.handshake-complete")
  private[jvm] val PeerFrameLimit = io.netty.util.AttributeKey.valueOf[java.lang.Long]("parrot.peer-frame-limit")

  final case class PeerHandshake(nodeId: String, capabilities: Long, maxFrameLen: Long)

  def decodeHandshake(body: Array[Byte], acknowledgement: Boolean): PeerHandshake = {
    // The public generic parser preserves every field. Network admission must
    // instead keep bounded metadata and never copy unknown or repeated values.
    val fields = scala.collection.mutable.Map.empty[Byte, Array[Byte]]
    var duplicate = false
    var p = 0
    while (p < body.length) {
      require(body.length - p >= 3, "truncated TLV header")
      val tag = body(p)
      val len = (body(p + 1).toInt & 0xff) | ((body(p + 2).toInt & 0xff) << 8)
      require(len <= body.length - p - 3, "truncated TLV value")
      if (tag >= TlvTag.NODE_ID && tag <= TlvTag.CHOSEN_CODEC) {
        if (fields.contains(tag)) duplicate = true
        else fields(tag) = java.util.Arrays.copyOfRange(body, p + 3, p + 3 + len)
      }
      p += 3 + len
    }
    // A truncated tail still takes precedence over an earlier duplicate.
    require(!duplicate, "duplicate handshake field")
    def required(tag: Byte, size: Int): Array[Byte] = {
      val value = fields.getOrElse(tag, throw new IllegalArgumentException("missing handshake field"))
      require(size < 0 || value.length == size, "invalid handshake field length")
      value
    }
    def string(bytes: Array[Byte]): String = strictUtf8(java.nio.ByteBuffer.wrap(bytes))
    val node = string(required(TlvTag.NODE_ID, -1))
    require(node.nonEmpty && !node.contains('\u0000'), "invalid node identity")
    for (tag <- Seq(TlvTag.REALM, TlvTag.CLUSTER, TlvTag.DIRECT_ADDR))
      fields.get(tag).foreach(string)
    val caps = readU32(required(TlvTag.CAPABILITIES, 4), 0)
    require((caps & CAPS_BIN_ONLY) != 0, "no common codec")
    val limit = readU32(required(TlvTag.MAX_FRAME_LEN, 4), 0)
    require(limit >= BODY_FIXED_OVERHEAD, "peer frame limit too small")
    require((required(TlvTag.TOPOLOGY_ROLE, 1)(0) & 0xff) <= 3, "invalid topology role")
    fields.get(TlvTag.HOP_LIMIT).foreach { bytes =>
      require(bytes.length == 1 && bytes(0) != 0, "invalid hop limit")
    }
    if (acknowledgement) {
      // Rust permits an omitted chosen_codec on older peers, defaulting to bin.
      require(fields.get(TlvTag.CHOSEN_CODEC).forall(v => string(v) == "bin"), "invalid chosen codec")
    } else require(!fields.contains(TlvTag.CHOSEN_CODEC), "chosen codec is ACK-only")
    PeerHandshake(node, caps, limit)
  }

  /** 兼容旧名（单测用）。 */
  def parseTlvNodeIds(body: Array[Byte]): List[(Byte, Array[Byte])] = parseTlv(body)
}
