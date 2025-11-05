package parrot.protocol.jvm

/** The bounded bincode 2 standard unsigned-u64 value used by the CPU probe.
  * This is not LEB128, and Java's signed Long cannot represent the whole range.
  */
private[jvm] object BincodeU64 {
  val Maximum: BigInt = (BigInt(1) << 64) - 1
  def decode(bytes: Array[Byte]): BigInt = {
    require(bytes != null && bytes.nonEmpty && bytes.length <= 9, "invalid bincode u64 size")
    val prefix = bytes(0).toInt & 0xff
    if (prefix <= 250) {
      require(bytes.length == 1, "trailing bincode bytes")
      BigInt(prefix)
    } else {
      val width = prefix match {
        case 251 => 2
        case 252 => 4
        case 253 => 8
        case _ => throw new IllegalArgumentException("invalid bincode u64 prefix")
      }
      require(bytes.length == width + 1, "truncated or trailing bincode u64")
      BigInt(1, bytes.drop(1).reverse)
    }
  }
  def encode(value: BigInt): Array[Byte] = {
    require(value >= 0 && value <= Maximum, "bincode u64 overflow")
    if (value <= 250) Array(value.toByte)
    else {
      val (tag, width) = if (value <= 65535) (251, 2) else if (value <= BigInt("ffffffff",16)) (252, 4) else (253, 8)
      Array(tag.toByte) ++ Array.tabulate[Byte](width)(i => ((value >> (8*i)) & 255).toByte)
    }
  }
}
