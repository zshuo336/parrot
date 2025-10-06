package parrot.protocol.jvm

import java.nio.charset.StandardCharsets.UTF_8

/** admin-v2 wire 编解码（DEV_09 B1/B5——与 Rust `admin_v2.rs` / Python
  * `admin_v2.py` / Erlang `parrot_gw.erl` 逐字节对齐）。
  *
  * payload 布局：`[u8 tag][bincode standard config(body)]`
  *   tag 0x03 = AdminCommandV2 / 0x04 = AdminReplyV2
  *
  * bincode standard varint：
  *   值 ≤ 250          → 单字节
  *   0xFB + u16 LE     （251 ..= 65535）
  *   0xFC + u32 LE     （65536 ..= 2^32-1）
  *   0xFD + u64 LE     （更大）
  * serde 形态：enum externally tagged = [varint 变体索引][各字段顺序体]；
  * Option = 0x00/0x01+T；String/Vec = varint len + 元素。
  * 冻结向量：docs/vectors/admin_v2.json（只增不改——修改 = 协议 break）。
  */
object AdminV2Codec {

  val TagAdminCmdV2: Int = 0x03
  val TagAdminReplyV2: Int = 0x04

  final class CodecException(msg: String) extends RuntimeException(msg)

  // ---------------- 镜像类型（Rust admin_v2.rs 同形） ----------------

  sealed trait ArtifactRef extends Product with Serializable
  object ArtifactRef {
    final case class Props(factory: String) extends ArtifactRef
    final case class Beam(app: String) extends ArtifactRef
    final case class PyModule(module: String, runtimeEnv: Option[String]) extends ArtifactRef
    final case class Jvm(mainClass: String, coords: Option[String]) extends ArtifactRef
    final case class Wasm(digest: String, uri: String) extends ArtifactRef
    final case class Dylib(digest: String, uri: String, abi: Long) extends ArtifactRef
  }

  sealed trait InstancePolicy extends Product with Serializable
  object InstancePolicy {
    case object Singleton extends InstancePolicy
    final case class Pool(count: Long) extends InstancePolicy
    final case class Sharded(count: Long) extends InstancePolicy
  }

  final case class ComponentDeploy(
      name: String,
      version: String,
      artifact: ArtifactRef,
      instances: InstancePolicy,
      config: Option[Array[Byte]]
  )

  sealed trait AdminCommandV2 { def reqId: Long }
  object AdminCommandV2 {
    final case class Deploy(reqId: Long, component: ComponentDeploy) extends AdminCommandV2
    final case class Drain(reqId: Long, pathPrefix: String, timeoutMs: Long) extends AdminCommandV2
    final case class Stop(reqId: Long, pathPrefix: String) extends AdminCommandV2
    final case class Status(reqId: Long, pathPrefix: String) extends AdminCommandV2
  }

  final case class ComponentState(path: String, state: String, version: String)

  sealed trait AdminReplyV2 { def reqId: Long }
  object AdminReplyV2 {
    final case class Deployed(reqId: Long, instances: Vector[String]) extends AdminReplyV2
    final case class Drained(reqId: Long, drained: Long, aborted: Long) extends AdminReplyV2
    final case class Stopped(reqId: Long) extends AdminReplyV2
    final case class StatusReply(reqId: Long, states: Vector[ComponentState]) extends AdminReplyV2
    final case class Failed(reqId: Long, code: Int, detail: String) extends AdminReplyV2
  }

  // ---------------- varint writer ----------------

  private final class W {
    private val b = scala.collection.mutable.ArrayBuffer.empty[Byte]
    def raw(x: Byte): Unit = b += x
    private def u16(v: Long): Unit = { b += (v & 0xFF).toByte; b += ((v >> 8) & 0xFF).toByte }
    private def u32(v: Long): Unit = { var i = 0; while (i < 4) { b += ((v >> (8 * i)) & 0xFF).toByte; i += 1 } }
    private def u64(v: Long): Unit = { var i = 0; while (i < 8) { b += ((v >> (8 * i)) & 0xFF).toByte; i += 1 } }
    def varint(v: Long): Unit = {
      if (v >= 0 && v <= 250) b += v.toByte
      else if (v >= 0 && v <= 0xFFFF) { b += 0xFB.toByte; u16(v) }
      else if (v >= 0 && v <= 0xFFFFFFFFL) { b += 0xFC.toByte; u32(v) }
      else { b += 0xFD.toByte; u64(v) }
    }
    def bytes(a: Array[Byte]): Unit = { varint(a.length); b ++= a }
    def str(s: String): Unit = bytes(s.getBytes(UTF_8))
    def optStr(o: Option[String]): Unit = o match {
      case None    => b += 0.toByte
      case Some(s) => b += 1.toByte; str(s)
    }
    def optBytes(o: Option[Array[Byte]]): Unit = o match {
      case None    => b += 0.toByte
      case Some(a) => b += 1.toByte; bytes(a)
    }
    def out(): Array[Byte] = b.toArray
  }

  // ---------------- varint reader ----------------

  private final class R(buf: Array[Byte], var pos: Int = 0) {
    private def need(n: Int): Unit =
      if (pos + n > buf.length)
        throw new CodecException(s"truncated at $pos (need $n, have ${buf.length - pos})")
    def byte(): Byte = { need(1); val v = buf(pos); pos += 1; v }
    def varint(): Long = {
      need(1)
      val b0 = buf(pos) & 0xFF
      if (b0 <= 0xFA) { pos += 1; b0.toLong }
      else if (b0 == 0xFB) {
        need(3)
        val v = (buf(pos + 1) & 0xFF) | ((buf(pos + 2) & 0xFF) << 8)
        pos += 3; v.toLong
      } else if (b0 == 0xFC) {
        need(5)
        var v = 0L; var i = 0
        while (i < 4) { v |= (buf(pos + 1 + i) & 0xFF).toLong << (8 * i); i += 1 }
        pos += 5; v
      } else if (b0 == 0xFD) {
        need(9)
        var v = 0L; var i = 0
        while (i < 8) { v |= (buf(pos + 1 + i) & 0xFF).toLong << (8 * i); i += 1 }
        pos += 9; v
      } else throw new CodecException(f"bad varint prefix 0x$b0%02x (0xFE/0xFF reserved)")
    }
    def bytes(): Array[Byte] = {
      val n = varint().toInt
      need(n)
      val a = java.util.Arrays.copyOfRange(buf, pos, pos + n)
      pos += n; a
    }
    def str(): String = new String(bytes(), UTF_8)
    def optStr(): Option[String] = byte() match {
      case 0 => None
      case 1 => Some(str())
      case t => throw new CodecException(s"bad Option tag $t")
    }
    def optBytes(): Option[Array[Byte]] = byte() match {
      case 0 => None
      case 1 => Some(bytes())
      case t => throw new CodecException(s"bad Option tag $t")
    }
    def done(): Unit =
      if (pos != buf.length) throw new CodecException(s"trailing bytes: consumed $pos of ${buf.length}")
  }

  // ---------------- 各复合体编解码 ----------------

  private def putArtifact(w: W, a: ArtifactRef): Unit = a match {
    case ArtifactRef.Props(factory) =>
      w.varint(0); w.str(factory)
    case ArtifactRef.Beam(app) =>
      w.varint(1); w.str(app)
    case ArtifactRef.PyModule(module, runtimeEnv) =>
      w.varint(2); w.str(module); w.optStr(runtimeEnv)
    case ArtifactRef.Jvm(mainClass, coords) =>
      w.varint(3); w.str(mainClass); w.optStr(coords)
    case ArtifactRef.Wasm(digest, uri) =>
      w.varint(4); w.str(digest); w.str(uri)
    case ArtifactRef.Dylib(digest, uri, abi) =>
      w.varint(5); w.str(digest); w.str(uri); w.varint(abi)
  }

  private def readArtifact(r: R): ArtifactRef = r.varint() match {
    case 0 => ArtifactRef.Props(r.str())
    case 1 => ArtifactRef.Beam(r.str())
    case 2 => val m = r.str(); ArtifactRef.PyModule(m, r.optStr())
    case 3 => val mc = r.str(); ArtifactRef.Jvm(mc, r.optStr())
    case 4 => val d = r.str(); ArtifactRef.Wasm(d, r.str())
    case 5 =>
      val d = r.str(); val u = r.str()
      ArtifactRef.Dylib(d, u, r.varint())
    case v => throw new CodecException(s"unknown artifact variant $v")
  }

  private def putPolicy(w: W, p: InstancePolicy): Unit = p match {
    case InstancePolicy.Singleton         => w.varint(0)
    case InstancePolicy.Pool(count)       => w.varint(1); w.varint(count)
    case InstancePolicy.Sharded(count)    => w.varint(2); w.varint(count)
  }

  private def readPolicy(r: R): InstancePolicy = r.varint() match {
    case 0 => InstancePolicy.Singleton
    case 1 => InstancePolicy.Pool(r.varint())
    case 2 => InstancePolicy.Sharded(r.varint())
    case v => throw new CodecException(s"unknown policy variant $v")
  }

  private def putComponent(w: W, c: ComponentDeploy): Unit = {
    w.str(c.name); w.str(c.version)
    putArtifact(w, c.artifact)
    putPolicy(w, c.instances)
    w.optBytes(c.config)
  }

  private def readComponent(r: R): ComponentDeploy = {
    val name = r.str(); val ver = r.str()
    val art = readArtifact(r); val pol = readPolicy(r)
    ComponentDeploy(name, ver, art, pol, r.optBytes())
  }

  // ---------------- AdminCommandV2 ----------------

  def encodeCommand(cmd: AdminCommandV2): Array[Byte] = {
    val w = new W
    w.raw(TagAdminCmdV2.toByte)
    cmd match {
      case AdminCommandV2.Deploy(reqId, component) =>
        w.varint(0); w.varint(reqId); putComponent(w, component)
      case AdminCommandV2.Drain(reqId, pathPrefix, timeoutMs) =>
        w.varint(1); w.varint(reqId); w.str(pathPrefix); w.varint(timeoutMs)
      case AdminCommandV2.Stop(reqId, pathPrefix) =>
        w.varint(2); w.varint(reqId); w.str(pathPrefix)
      case AdminCommandV2.Status(reqId, pathPrefix) =>
        w.varint(3); w.varint(reqId); w.str(pathPrefix)
    }
    w.out()
  }

  def decodeCommand(payload: Array[Byte]): AdminCommandV2 = {
    if (payload.isEmpty) throw new CodecException("empty payload")
    if ((payload(0) & 0xFF) != TagAdminCmdV2)
      throw new CodecException(f"bad tag 0x${payload(0) & 0xFF}%02x (expect 0x03)")
    val r = new R(payload, 1)
    val out = r.varint() match {
      case 0 =>
        val reqId = r.varint()
        AdminCommandV2.Deploy(reqId, readComponent(r))
      case 1 =>
        val reqId = r.varint(); val prefix = r.str()
        AdminCommandV2.Drain(reqId, prefix, r.varint())
      case 2 =>
        val reqId = r.varint()
        AdminCommandV2.Stop(reqId, r.str())
      case 3 =>
        val reqId = r.varint()
        AdminCommandV2.Status(reqId, r.str())
      case v => throw new CodecException(s"unknown cmd variant $v")
    }
    r.done()
    out
  }

  // ---------------- AdminReplyV2 ----------------

  def encodeReply(rep: AdminReplyV2): Array[Byte] = {
    val w = new W
    w.raw(TagAdminReplyV2.toByte)
    rep match {
      case AdminReplyV2.Deployed(reqId, instances) =>
        w.varint(0); w.varint(reqId)
        w.varint(instances.length)
        instances.foreach(w.str)
      case AdminReplyV2.Drained(reqId, drained, aborted) =>
        w.varint(1); w.varint(reqId); w.varint(drained); w.varint(aborted)
      case AdminReplyV2.Stopped(reqId) =>
        w.varint(2); w.varint(reqId)
      case AdminReplyV2.StatusReply(reqId, states) =>
        w.varint(3); w.varint(reqId)
        w.varint(states.length)
        states.foreach { s => w.str(s.path); w.str(s.state); w.str(s.version) }
      case AdminReplyV2.Failed(reqId, code, detail) =>
        w.varint(4); w.varint(reqId); w.varint(code.toLong); w.str(detail)
    }
    w.out()
  }

  def decodeReply(payload: Array[Byte]): AdminReplyV2 = {
    if (payload.isEmpty) throw new CodecException("empty payload")
    if ((payload(0) & 0xFF) != TagAdminReplyV2)
      throw new CodecException(f"bad tag 0x${payload(0) & 0xFF}%02x (expect 0x04)")
    val r = new R(payload, 1)
    val out = r.varint() match {
      case 0 =>
        val reqId = r.varint()
        val n = r.varint().toInt
        val instances = (0 until n).map(_ => r.str()).toVector
        AdminReplyV2.Deployed(reqId, instances)
      case 1 =>
        val reqId = r.varint()
        AdminReplyV2.Drained(reqId, r.varint(), r.varint())
      case 2 =>
        AdminReplyV2.Stopped(r.varint())
      case 3 =>
        val reqId = r.varint()
        val n = r.varint().toInt
        val states = (0 until n).map { _ =>
          val p = r.str(); val s = r.str()
          ComponentState(p, s, r.str())
        }.toVector
        AdminReplyV2.StatusReply(reqId, states)
      case 4 =>
        val reqId = r.varint(); val code = r.varint()
        AdminReplyV2.Failed(reqId, code.toInt, r.str())
      case v => throw new CodecException(s"unknown reply variant $v")
    }
    r.done()
    out
  }

  /** SYSTEM_EVENT payload 首字节是否 admin-v2 命令（transport 分发判定）。 */
  def isCmdPayload(payload: Array[Byte]): Boolean =
    payload.nonEmpty && (payload(0) & 0xFF) == TagAdminCmdV2
}
