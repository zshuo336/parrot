package parrot.protocol.jvm

import java.lang.management.ManagementFactory
import org.scalatest.funsuite.AnyFunSuite

class HandshakeResourcesSpec extends AnyFunSuite {
  private def tlv(tag: Int, value: Array[Byte]): Array[Byte] =
    Array(tag.toByte, value.length.toByte, (value.length >> 8).toByte) ++ value
  private def hello(ack: Boolean): Array[Byte] =
    tlv(1, Array[Byte](112, 101, 101, 114)) ++ tlv(4, Array[Byte](1, 0, 0, 0)) ++
      tlv(5, Array[Byte](0, 0, 16, 0)) ++ tlv(6, Array[Byte](0)) ++
      (if (ack) tlv(9, Array[Byte](98, 105, 110)) else Array.emptyByteArray)
  private def flood(tag: Int, value: Array[Byte]): Array[Byte] = {
    val field = tlv(tag, value)
    val out = new Array[Byte](field.length * 32768)
    for (n <- 0 until 32768) System.arraycopy(field, 0, out, n * field.length, field.length)
    out
  }
  private def decode(body: Array[Byte], ack: Boolean): Either[String, WireFrame.PeerHandshake] =
    try Right(WireFrame.decodeHandshake(body, ack))
    catch { case e: IllegalArgumentException => Left(e.getMessage) }

  private def bounded(body: Array[Byte], ack: Boolean): Either[String, WireFrame.PeerHandshake] = {
    val meter = ManagementFactory.getThreadMXBean match {
      case m: com.sun.management.ThreadMXBean if m.isThreadAllocatedMemorySupported => m
      case _ => cancel("resource gate requires a JVM with per-thread allocation accounting")
    }
    if (!meter.isThreadAllocatedMemoryEnabled) meter.setThreadAllocatedMemoryEnabled(true)
    // Warm class initialization outside accounting; no fixture construction or
    // ScalaTest assertions are included in the allocation delta.
    for (_ <- 0 until 4) decode(body, ack)
    val thread = Thread.currentThread().threadId()
    val before = meter.getThreadAllocatedBytes(thread)
    val result = decode(body, ack)
    val allocated = meter.getThreadAllocatedBytes(thread) - before
    assert(allocated >= 0 && allocated < 64 * 1024, s"decoder allocated $allocated bytes")
    result
  }

  test("unknown empty and payload-bearing floods do not allocate per-field storage") {
    for (ack <- Seq(false, true); value <- Seq(Array.emptyByteArray, Array.fill[Byte](64)(42))) {
      val allUnknown = (Seq(0) ++ (10 until 255)).flatMap(tag => tlv(tag, Array[Byte](-1))).toArray
      val body = hello(ack) ++ flood(255, value) ++ allUnknown
      assert(bounded(body, ack) == decode(hello(ack), ack))
    }
  }

  test("every known duplicate field rejects with bounded allocation") {
    for (tag <- 1 to 9) {
      val body = hello(true) ++ flood(tag, Array[Byte](42))
      assert(bounded(body, true).left.toOption.exists(_.contains("duplicate handshake field")))
    }
  }

  test("truncated unknown and known tails reject before duplicate semantics") {
    for (ack <- Seq(false, true); tail <- Seq(Array[Byte](-1), Array[Byte](-1, 1),
      Array[Byte](-1, 2, 0, 42), Array[Byte](1, 2, 0, 42))) {
      val body = hello(ack) ++ flood(255, Array[Byte](42)) ++ tail
      assert(bounded(body, ack).left.toOption.exists(_.contains("truncated TLV")))
    }
    val duplicate = hello(false) ++ flood(1, Array[Byte](42)) ++ Array[Byte](-1)
    assert(bounded(duplicate, false).left.toOption.exists(_.contains("truncated TLV")))
  }

  test("public TLV parser preserves every owned field including repeated unknown tags") {
    val entries = List((255, Array[Byte](1)), (1, Array[Byte](2)), (255, Array[Byte](3)), (0, Array.emptyByteArray))
    val body = entries.flatMap { case (tag, value) => tlv(tag, value) }.toArray
    val parsed = WireFrame.parseTlv(body)
    assert(parsed.map(_._1 & 0xff) == entries.map(_._1))
    assert(parsed.zip(entries).forall { case ((_, actual), (_, expected)) => actual.sameElements(expected) })
    java.util.Arrays.fill(body, 0.toByte)
    assert(parsed.head._2.sameElements(Array[Byte](1)))
    intercept[IllegalArgumentException] { WireFrame.parseTlv(Array[Byte](-1)) }
  }
}
