package parrot.protocol.jvm

import java.nio.ByteBuffer
import java.nio.charset.{CodingErrorAction, StandardCharsets, CharacterCodingException}

/** Strict little-endian application decoding. Unsigned lengths are checked before
  * conversion/allocation; finish must precede any application state mutation. */
final class PayloadReader(payload: Array[Byte], maxBytes: Int = WireFrame.MAX_FRAME_LEN) {
  import PayloadReader.InvalidPayload
  if (payload.length > maxBytes) throw new InvalidPayload("application payload exceeds byte budget")
  private var offset = 0
  private def take(size: Long): Int = {
    if (size < 0 || size > payload.length.toLong - offset)
      throw new InvalidPayload("truncated application payload")
    val start = offset
    offset += size.toInt
    start
  }
  def u32(): Long = {
    val start = take(4)
    (0 until 4).foldLeft(0L)((v, i) => v | ((payload(start + i).toLong & 255L) << (8 * i)))
  }
  /** All 64 bits are preserved; application doc IDs retain their signed JSON representation. */
  def u64(): Long = {
    val start = take(8)
    (0 until 8).foldLeft(0L)((v, i) => v | ((payload(start + i).toLong & 255L) << (8 * i)))
  }
  def count(minItemBytes: Int, maxItems: Int = 65536): Int = {
    require(minItemBytes > 0 && maxItems >= 0)
    val n = u32()
    if (n > maxItems) throw new InvalidPayload("application payload exceeds item budget")
    if (n > (payload.length - offset) / minItemBytes)
      throw new InvalidPayload("truncated application item collection")
    n.toInt
  }
  def utf8(): String = {
    val size = u32()
    val start = take(size)
    val decoder = StandardCharsets.UTF_8.newDecoder()
      .onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT)
    try decoder.decode(ByteBuffer.wrap(payload, start, size.toInt)).toString
    catch { case _: CharacterCodingException => throw new InvalidPayload("invalid application UTF-8") }
  }
  def finish(): Unit = if (offset != payload.length)
    throw new InvalidPayload("trailing application payload bytes")
}

object PayloadReader {
  final class InvalidPayload(message: String) extends IllegalArgumentException(message)
}
