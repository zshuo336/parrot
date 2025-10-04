package parrot.protocol.jvm

/** pb 桥编解码（与 parrot-remote pb.rs AkkaEnvelope 字节级一致）。
  *
  * .proto 等价：
  * {{{
  * package parrot.protocol.v1;
  * message AkkaEnvelope {
  *   string akka_path = 1;
  *   string kind = 2;
  *   bytes payload = 3;
  * }
  * }}}
  * TYPE_KEY: "pb:parrot.protocol.v1.AkkaEnvelope"（两端同键）。
  */
object Codec {
  val AkkaEnvelopeKey = "pb:parrot.protocol.v1.AkkaEnvelope"

  final case class AkkaEnvelope(akkaPath: String, kind: String, payload: Array[Byte])

  def encodeEnvelope(e: AkkaEnvelope): Array[Byte] = {
    val path = e.akkaPath.getBytes("UTF-8")
    val kind = e.kind.getBytes("UTF-8")
    var size = 0
    size += fieldSize(1, path.length)
    size += fieldSize(2, kind.length)
    size += fieldSize(3, e.payload.length)
    val out = new Array[Byte](size)
    var p = 0
    p = writeTag(out, p, 1, 2); p = writeBytes(out, p, path)
    p = writeTag(out, p, 2, 2); p = writeBytes(out, p, kind)
    p = writeTag(out, p, 3, 2); p = writeBytes(out, p, e.payload)
    out
  }

  def decodeEnvelope(b: Array[Byte]): Option[AkkaEnvelope] = {
    var p = 0
    var path: Option[String] = None
    var kind: Option[String] = None
    var payload: Option[Array[Byte]] = None
    def uvarint(): Long = {
      var shift = 0L; var v = 0L
      while (p < b.length) {
        val byte = b(p).toInt & 0xFF
        p += 1
        v |= ((byte & 0x7F).toLong) << shift
        if ((byte & 0x80) == 0) return v
        shift += 7
      }
      v
    }
    try {
      while (p < b.length) {
        val tag = uvarint()
        val field = (tag >> 3).toInt
        val wire = (tag & 7).toInt
        if (wire == 2) {
          val len = uvarint().toInt
          val bytes = java.util.Arrays.copyOfRange(b, p, p + len)
          p += len
          field match {
            case 1 => path = Some(new String(bytes, "UTF-8"))
            case 2 => kind = Some(new String(bytes, "UTF-8"))
            case 3 => payload = Some(bytes)
            case _ => // 未知字段跳过（前向兼容）
          }
        } else if (wire == 0) {
          uvarint() // varint 字段跳过
        } else return None // wire 5/1 不在本消息
      }
      Some(AkkaEnvelope(path.getOrElse(""), kind.getOrElse(""), payload.getOrElse(Array.emptyByteArray)))
    } catch { case _: Throwable => None }
  }

  private def fieldSize(fieldNo: Int, len: Int): Int =
    tagSize(fieldNo) + varintSize(len.toLong) + len

  private def tagSize(fieldNo: Int): Int = varintSize((fieldNo.toLong << 3) | 2)

  private def varintSize(v: Long): Int = {
    var n = 1
    var x = v >>> 7
    while (x != 0) { n += 1; x >>>= 7 }
    n
  }

  private def writeTag(out: Array[Byte], p: Int, fieldNo: Int, wire: Int): Int =
    writeUvarint(out, p, ((fieldNo.toLong << 3) | wire))

  private def writeUvarint(out: Array[Byte], p: Int, v0: Long): Int = {
    var p2 = p; var v = v0
    while ((v & ~0x7FL) != 0) { out(p2) = ((v & 0x7F) | 0x80).toByte; v >>>= 7; p2 += 1 }
    out(p2) = v.toByte
    p2 + 1
  }

  private def writeBytes(out: Array[Byte], p: Int, b: Array[Byte]): Int = {
    System.arraycopy(b, 0, out, p, b.length)
    p + b.length
  }
}
