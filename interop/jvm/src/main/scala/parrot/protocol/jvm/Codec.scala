package parrot.protocol.jvm

import com.google.protobuf.CodedOutputStream
import java.nio.{ByteBuffer, CharBuffer}
import java.nio.charset.{CharacterCodingException, CodingErrorAction, StandardCharsets}
import scala.util.control.NonFatal

/** Protobuf AkkaEnvelope: string akka_path=1, string kind=2, bytes payload=3.
  * Matches Rust prost, including proto3 defaults, unknown fields and last-value
  * merge semantics. This library codec does not itself install a gateway route.
  */
object Codec {
  val AkkaEnvelopeKey = "pb:parrot.protocol.v1.AkkaEnvelope"
  val MaxEnvelopeBytes: Int = WireFrame.MAX_FRAME_LEN
  private val MaxGroupDepth = 100
  final case class AkkaEnvelope(akkaPath: String, kind: String, payload: Array[Byte])

  private def utf8(value: String): Array[Byte] = {
    require(value != null && value.length <= MaxEnvelopeBytes, "invalid envelope string size")
    try {
      val encoded = StandardCharsets.UTF_8.newEncoder()
        .onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT)
        .encode(CharBuffer.wrap(value))
      val bytes = new Array[Byte](encoded.remaining())
      encoded.get(bytes)
      bytes
    } catch { case e: CharacterCodingException => throw new IllegalArgumentException("invalid envelope UTF16", e) }
  }

  def encodeEnvelope(e: AkkaEnvelope): Array[Byte] = {
    require(e != null && e.payload != null && e.payload.length <= MaxEnvelopeBytes, "invalid envelope payload size")
    val fields = Array(utf8(e.akkaPath), utf8(e.kind), e.payload)
    val size = fields.zipWithIndex.foldLeft(0L) { case (total, (bytes, index)) =>
      total + (if (bytes.isEmpty) 0L else CodedOutputStream.computeByteArraySize(index + 1, bytes).toLong)
    }
    require(size <= MaxEnvelopeBytes, "envelope exceeds byte limit")
    val bytes = new Array[Byte](size.toInt)
    val out = CodedOutputStream.newInstance(bytes)
    fields.zipWithIndex.foreach { case (value, index) =>
      if (value.nonEmpty) out.writeByteArray(index + 1, value)
    }
    out.checkNoSpaceLeft()
    bytes
  }

  def decodeEnvelope(b: Array[Byte]): Option[AkkaEnvelope] = {
    if (b == null || b.length > MaxEnvelopeBytes) return None
    // Check widths before narrowing and lengths before allocation. Some generic
    // protobuf readers truncate oversized uint32 keys/lengths; prost rejects them.
    final class Reader {
      var position = 0
      def available: Int = b.length - position
      def advance(size: Int): Unit = {
        require(size >= 0 && size <= available, "truncated protobuf value")
        position += size
      }
      def varint(): Long = {
        var value = 0L
        var i = 0
        while (i < 10) {
          require(available > 0, "truncated protobuf varint")
          val byte = b(position).toInt & 0xff
          position += 1
          require(i != 9 || byte <= 1, "overflowing protobuf varint")
          value |= (byte & 0x7f).toLong << (7 * i)
          if ((byte & 0x80) == 0) return value
          i += 1
        }
        throw new IllegalArgumentException("unterminated protobuf varint")
      }
      def tag(): Long = {
        val value = varint()
        require(value > 0 && value <= 0xffffffffL && (value >>> 3) != 0 && (value & 7) <= 5, "invalid protobuf tag")
        value
      }
      def length(): Int = {
        val value = varint()
        require(value >= 0 && value <= available, "invalid protobuf length")
        value.toInt
      }
      def text(): String = {
        val size = length()
        val result = StandardCharsets.UTF_8.newDecoder()
          .onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT)
          .decode(ByteBuffer.wrap(b, position, size)).toString
        advance(size)
        result
      }
      def bytes(): Array[Byte] = {
        val size = length()
        val result = java.util.Arrays.copyOfRange(b, position, position + size)
        advance(size)
        result
      }
      def skip(current: Long, depth: Int): Unit = (current & 7).toInt match {
        case 0 => varint(); ()
        case 1 => advance(8)
        case 2 => advance(length())
        case 3 =>
          require(depth < MaxGroupDepth, "protobuf group nesting limit")
          var complete = false
          while (!complete) {
            val next = tag()
            if ((next & 7) == 4) {
              require((next >>> 3) == (current >>> 3), "mismatched protobuf group")
              complete = true
            } else skip(next, depth + 1)
          }
        case 5 => advance(4)
        case _ => throw new IllegalArgumentException("unexpected protobuf end group")
      }
    }
    try {
      val in = new Reader
      var path = ""
      var kind = ""
      var payload = Array.emptyByteArray
      while (in.available > 0) {
        val tag = in.tag()
        (tag >>> 3).toInt match {
          case 1 => require((tag & 7) == 2, "invalid path wire type"); path = in.text()
          case 2 => require((tag & 7) == 2, "invalid kind wire type"); kind = in.text()
          case 3 => require((tag & 7) == 2, "invalid payload wire type"); payload = in.bytes()
          case _ => in.skip(tag, 0)
        }
      }
      Some(AkkaEnvelope(path, kind, payload))
    } catch { case NonFatal(_) => None }
  }
}
