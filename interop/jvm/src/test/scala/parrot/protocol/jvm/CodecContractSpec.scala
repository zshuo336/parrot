package parrot.protocol.jvm

import org.scalatest.funsuite.AnyFunSuite
import scala.util.Using
import scala.io.Source

class CodecContractSpec extends AnyFunSuite {
  private def hex(text: String): Array[Byte] = text.grouped(2).map(Integer.parseInt(_,16).toByte).toArray
  private val corpus = Using.resource(Source.fromFile("../../docs/vectors/jvm-codecs.tsv")) { source =>
    source.getLines().filterNot(_.startsWith("#")).map(_.split("\t",-1)).filter(_(0)=="pb").toVector
  }
  corpus.foreach { row =>
    val Array(_,name,input,expected)=row
    test(s"protobuf matches independent Rust prost vector $name") {
      val decoded=Codec.decodeEnvelope(hex(input))
      if(expected=="ERR") assert(decoded.isEmpty)
      else {
        assert(decoded.isDefined)
        assert(Codec.encodeEnvelope(decoded.get).sameElements(hex(expected)))
      }
    }
  }
  test("encoder emits independently specified protobuf length-delimited fields") {
    val bytes=Codec.encodeEnvelope(Codec.AkkaEnvelope("a","b",Array[Byte](3)))
    assert(bytes.sameElements(hex("0a01611201621a0103")))
  }
  test("valid protobuf field-boundary prefixes are accepted but partial fields are rejected") {
    val bytes=hex("0a01611201621a0103")
    for(end <- 0 to bytes.length) {
      val decoded=Codec.decodeEnvelope(bytes.take(end))
      assert(decoded.isDefined==Set(0,3,6,9).contains(end),s"prefix=$end")
      decoded.foreach(value => assert(Codec.encodeEnvelope(value).sameElements(bytes.take(end))))
    }
  }
  test("invalid UTF16 is rejected rather than silently replaced") {
    for(value <- Seq(Codec.AkkaEnvelope("\ud800","",Array.emptyByteArray),Codec.AkkaEnvelope("","\udfff",Array.emptyByteArray)))
      intercept[IllegalArgumentException] { Codec.encodeEnvelope(value) }
  }
  test("payload budget rejects before allocation from declared length and bounds encoding") {
    assert(Codec.decodeEnvelope(hex("0a81808008")).isEmpty)
    val oversized=Array.ofDim[Byte](WireFrame.MAX_FRAME_LEN)
    intercept[IllegalArgumentException] { Codec.encodeEnvelope(Codec.AkkaEnvelope("","",oversized)) }
    assert(Codec.decodeEnvelope(oversized :+ 0.toByte).isEmpty)
  }
  test("unknown group nesting is bounded and unterminated groups fail") {
    assert(Codec.decodeEnvelope(Array.fill[Byte](1001)(35) ++ Array.fill[Byte](1001)(36)).isEmpty)
    assert(Codec.decodeEnvelope(Array[Byte](35,35,36)).isEmpty)
  }
  test("exact envelope byte budget remains accepted") {
    val payload=Array.fill[Byte](Codec.MaxEnvelopeBytes-5)(7)
    val encoded=Codec.encodeEnvelope(Codec.AkkaEnvelope("","",payload))
    assert(encoded.length==Codec.MaxEnvelopeBytes)
    assert(Codec.decodeEnvelope(encoded).get.payload.sameElements(payload))
  }

}
