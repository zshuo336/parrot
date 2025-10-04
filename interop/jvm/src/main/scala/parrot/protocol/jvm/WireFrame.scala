package parrot.protocol.jvm

import java.nio.charset.StandardCharsets.UTF_8

/** Wire 1.0 帧（与 parrot-remote frame.rs 字节级一致）。
  *
  * 布局：[u32 frame_len][u8 version][u8 frame_type][u16 flags][u64 cid]
  *       [u8 hop_count][u8 hop_limit][u48 reserved=0][u32 path_len][path]
  *       [u32 key_len][key][payload]；全部 LE。
  *
  * frame_len = 24B 定长 + path/key/payload（不含自身 4B 前缀）。
  */
object WireFrame {
  val PROTOCOL_VERSION: Byte = 0x01
  val MAX_FRAME_LEN = 16 * 1024 * 1024
  val BODY_FIXED_OVERHEAD = 28 // 24 定长 + 4 path_len

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
    val ERROR: Byte = 0x7F
  }

  final case class Frame(
      version: Byte,
      frameType: Byte,
      flags: Int,
      correlationId: Long,
      hopCount: Byte,
      hopLimit: Byte,
      path: String,
      typeKey: String,
      payload: Array[Byte]
  ) {
    def encode(): Array[Byte] = {
      val pathB = path.getBytes(UTF_8)
      val keyB  = typeKey.getBytes(UTF_8)
      val body  = BODY_FIXED_OVERHEAD + pathB.length + keyB.length + payload.length
      val out   = new Array[Byte](4 + body)
      writeU32(out, 0, body)
      out(4) = version
      out(5) = frameType
      out(6) = (flags & 0xFF).toByte
      out(7) = ((flags >> 8) & 0xFF).toByte
      writeU64(out, 8, correlationId)
      out(16) = hopCount
      out(17) = hopLimit
      // reserved u48 = 0（18..24）
      var i = 18
      while (i < 24) { out(i) = 0; i += 1 }
      writeU32(out, 24, pathB.length)
      System.arraycopy(pathB, 0, out, 28, pathB.length)
      var p = 28 + pathB.length
      writeU32(out, p, keyB.length)
      p += 4
      System.arraycopy(keyB, 0, out, p, keyB.length)
      p += keyB.length
      System.arraycopy(payload, 0, out, p, payload.length)
      out
    }
  }

  private def readU32(b: Array[Byte], off: Int): Long =
    (b(off).toLong & 0xFF) | ((b(off + 1).toLong & 0xFF) << 8) |
      ((b(off + 2).toLong & 0xFF) << 16) | ((b(off + 3).toLong & 0xFF) << 24)

  private def readU64(b: Array[Byte], off: Int): Long = {
    var v = 0L
    var i = 7
    while (i >= 0) { v = (v << 8) | (b(off + i).toLong & 0xFF); i -= 1 }
    v
  }

  private def writeU32(b: Array[Byte], off: Int, v: Long): Unit = {
    b(off) = (v & 0xFF).toByte
    b(off + 1) = ((v >> 8) & 0xFF).toByte
    b(off + 2) = ((v >> 16) & 0xFF).toByte
    b(off + 3) = ((v >> 24) & 0xFF).toByte
  }

  private def writeU64(b: Array[Byte], off: Int, v: Long): Unit = {
    var i = 0
    while (i < 8) { b(off + i) = ((v >> (8 * i)) & 0xFF).toByte; i += 1 }
  }

  /** 半包语义：不足一帧返回 None 且不消费（readerIndex 由调用方管理——
    * Netty Decoder 形态：返回解码字节数 0 = 未消费）。
    */
  def decode(buf: Array[Byte]): Option[(Frame, Int)] = {
    if (buf.length < 4) return None
    val bodyLen = readU32(buf, 0).toInt
    if (bodyLen < 0 || bodyLen > MAX_FRAME_LEN) return None // 超限视为坏帧
    if (buf.length < 4 + bodyLen) return None
    val version = buf(4)
    if (version != PROTOCOL_VERSION) return None
    val ft      = buf(5)
    val flags   = (buf(6).toInt & 0xFF) | ((buf(7).toInt & 0xFF) << 8)
    val cid     = readU64(buf, 8)
    val hopCnt  = buf(16)
    val hopLmt  = buf(17)
    val pathLen = readU32(buf, 24).toInt
    val path    = new String(buf, 28, pathLen, UTF_8)
    var p       = 28 + pathLen
    val keyLen  = readU32(buf, p).toInt
    p += 4
    val key     = new String(buf, p, keyLen, UTF_8)
    p += keyLen
    val payloadLen = bodyLen - BODY_FIXED_OVERHEAD - pathLen - keyLen
    val payload    = java.util.Arrays.copyOfRange(buf, p, p + payloadLen)
    Some((Frame(version, ft, flags, cid, hopCnt, hopLmt, path, key, payload), 4 + bodyLen))
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
    val d = detail.getBytes(UTF_8)
    val out = new Array[Byte](4 + d.length)
    out(0) = (code & 0xFF).toByte
    out(1) = ((code >> 8) & 0xFF).toByte
    out(2) = 0
    out(3) = 0
    System.arraycopy(d, 0, out, 4, d.length)
    out
  }

  def decodeErrPayload(b: Array[Byte]): (Int, String) =
    if (b.length < 4) (ErrCode.ProtocolViolation, "<undecodable>")
    else {
      val code = (b(0).toInt & 0xFF) | ((b(1).toInt & 0xFF) << 8)
      (code, new String(b, 4, b.length - 4, UTF_8))
    }

  // ---------------- ASK reply_to 前缀（与 frame.rs split_reply_to 一致） ----------------

  def withReplyToPrefix(replyTo: String, payload: Array[Byte]): Array[Byte] = {
    val rb = replyTo.getBytes(UTF_8)
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
      val rlen = readU32(b, 0).toInt
      if (b.length < 4 + rlen) None
      else {
        val rto = new String(b, 4, rlen, UTF_8)
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
    val CHOSEN_CODEC: Byte = 8
  }

  /** 能力位：JVM 网关只说 bincode 栈（bit0）。 */
  val CAPS_BIN_ONLY: Int = 0x01

  def handshakeBody(nodeId: String): Array[Byte] = {
    val id = nodeId.getBytes(UTF_8)
    val bb = java.nio.ByteBuffer
      .allocate(3 + id.length + 3 + 4 + 3 + 4 + 3 + 1 + 3 + 1)
      .order(java.nio.ByteOrder.LITTLE_ENDIAN)
    bb.put(TlvTag.NODE_ID); bb.putShort(id.length.toShort); bb.put(id)
    bb.put(TlvTag.CAPABILITIES); bb.putShort(4.toShort); bb.putInt(CAPS_BIN_ONLY)
    bb.put(TlvTag.MAX_FRAME_LEN); bb.putShort(4.toShort); bb.putInt(MAX_FRAME_LEN)
    bb.put(TlvTag.TOPOLOGY_ROLE); bb.putShort(1.toShort); bb.put(0.toByte) // normal
    bb.put(TlvTag.HOP_LIMIT); bb.putShort(1.toShort); bb.put(8.toByte)
    bb.array()
  }

  /** ACK 体：同族字段 + chosen_codec=bin（1.0 固定 bin 优先）。 */
  def handshakeAckBody(nodeId: String): Array[Byte] = {
    val base = handshakeBody(nodeId)
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
    while (p + 3 <= body.length) {
      val tag = body(p)
      val len = (body(p + 1).toInt & 0xFF) | ((body(p + 2).toInt & 0xFF) << 8)
      if (p + 3 + len > body.length) return out.toList
      out += ((tag, java.util.Arrays.copyOfRange(body, p + 3, p + 3 + len)))
      p += 3 + len
    }
    out.toList
  }

  /** 兼容旧名（单测用）。 */
  def parseTlvNodeIds(body: Array[Byte]): List[(Byte, Array[Byte])] = parseTlv(body)
}
