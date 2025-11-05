package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite

class PayloadReaderSpec extends AnyFunSuite {
  test("unsigned scalars and UTF8 independently encoded bytes consume exactly") {
    val bytes = Array.fill[Byte](12)(-1) ++ Array[Byte](4, 0, 0, 0, -16, -97, -104, -128)
    val reader = new PayloadReader(bytes)
    assert(reader.u32() == 4294967295L)
    assert(reader.u64() == -1L)
    assert(reader.utf8() == "😀")
    reader.finish()
    bytes.indices.foreach { end =>
      intercept[PayloadReader.InvalidPayload] {
        val r = new PayloadReader(bytes.take(end)); r.u32(); r.u64(); r.utf8(); r.finish()
      }
    }
  }
  test("collections are bounded by policy and actual remaining bytes before allocation") {
    intercept[PayloadReader.InvalidPayload] { new PayloadReader(Array[Byte](2, 0, 0, 0, 1)).count(1) }
    intercept[PayloadReader.InvalidPayload] { new PayloadReader(Array[Byte](2, 0, 0, 0, 1, 2)).count(1, 1) }
    assert(new PayloadReader(Array[Byte](2, 0, 0, 0, 1, 2)).count(1) == 2)
    intercept[PayloadReader.InvalidPayload] { new PayloadReader(Array[Byte](0), 0) }
    intercept[IllegalArgumentException] { new PayloadReader(Array[Byte](0, 0, 0, 0)).count(0) }
  }
  test("trailing bytes and malformed UTF8 cannot be normalized to successful input") {
    intercept[PayloadReader.InvalidPayload] { new PayloadReader(Array[Byte](0)).finish() }
    for (bytes <- Vector(Array[Byte](1, 0, 0, 0, -1), Array[Byte](2, 0, 0, 0, -64, -128), Array.fill[Byte](4)(-1))) {
      intercept[PayloadReader.InvalidPayload] { new PayloadReader(bytes).utf8() }
    }
    new PayloadReader(Array.emptyByteArray).finish()
  }
}
