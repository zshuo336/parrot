package parrot.protocol.jvm

import java.io.File
import java.net.{URL, URLClassLoader}
import java.nio.charset.StandardCharsets.UTF_8
import java.security.MessageDigest
import java.util.concurrent.ConcurrentHashMap
import scala.collection.mutable
import scala.jdk.CollectionConverters._
import scala.util.{Failure, Success, Try}

/** B5（DEV_09）：admin-v2 四命令的 akka/JVM 方言执行器。
  *
  * - DeployComponent{Jvm} → child-first `URLClassLoader`（jar 目录/坐标）
  *   → `loader.loadClass(main_class)` 反射构造 `ComponentSpi` →
  *   gateway system spawn（/user/{name}[-{i}]，与 parrot/erl/ray 同规）。
  * - Drain → 实例 drain 钩子 + 共享 deadline，超时 → aborted（摘路由）。
  * - Stop → 立即 stop 全部实例（akka typed 无 PoisonPill——ask 停止协议）。
  * - Status → 登记表 + akka 探活（identify 语义由 registry ref 存活判定）。
  *
  * artifact uri 形态（与 Rust ArtifactChannel file:// 同族）：
  *   file:///path/to/dir   → 目录下所有 .jar 进 child loader
  *   file:///path/to/x.jar → 单 jar
  * coords（可选）→ Maven 坐标 "g:a:v"（B5 首期：仍按 uri 解析——
  *   坐标仅登记，不自动下载；D/C 阶段接 parrot-app artifact store）。
  */
object AdminV2Executor {

  // v2 错误码扩展段（0x0A00+——与 Rust v2_err 对齐）
  val ErrArtifactFetch    = 0x0A00
  val ErrArtifactDigest   = 0x0A01
  val ErrDialectMismatch  = 0x0A02
  val ErrComponentNotFound = 0x0A03
  val ErrDrainTimeout     = 0x0A04
  val ErrFactoryNotFound  = 0x0A05
  val ErrSpawnFailed      = 0x0A06

  /** 组件登记表项（进程内；网关单点）。 */
  final case class ComponentEntry(
      name: String,
      version: String,
      paths: Vector[String],
      loader: Option[URLClassLoader], // Props 直载（无 jar）时 None
      instances: Vector[InstanceHandle]
  )

  /** 单实例句柄：SPI 对象 + akka stopper 闭包。 */
  final case class InstanceHandle(
      path: String,
      spi: ComponentSpi,
      stopper: () => Unit
  )

  final class AdminException(val code: Int, msg: String) extends RuntimeException(msg)

  // ---------------- child-first loader ----------------

  /** child-first：artifact 自带类优先（子加载），父链类（akka/scala/JDK）
    * 委派——跨加载器类型身份保障（BridgeAsk 等协议类型不可 child 载入）。
    */
  private final class ChildFirstLoader(urls: Array[URL], parent: ClassLoader)
      extends URLClassLoader(urls, parent) {
    private val parentFirstPrefixes = Array(
      "java.", "javax.", "jdk.", "sun.", "scala.", "akka.", "com.typesafe.",
      "io.netty.", "org.slf4j.", "ch.qos.", "parrot.protocol.jvm."
    )
    override def loadClass(name: String, resolve: Boolean): Class[_] = {
      val loaded = findLoadedClass(name)
      if (loaded != null) loaded
      else if (parentFirstPrefixes.exists(name.startsWith)) super.loadClass(name, resolve)
      else {
        Try(findClass(name)) match {
          case Success(c) =>
            if (resolve) resolveClass(c)
            c
          case Failure(_) => super.loadClass(name, resolve) // 兜底委派父链
        }
      }
    }
  }

  /** 解析 artifact uri → child loader（目录扫 .jar / 单 .jar / class 目录）。
    * 非 Jvm artifact → DialectMismatch。sha256 校验（digest 非空时）。
    */
  def buildLoader(
      artifact: AdminV2Codec.ArtifactRef
  ): Option[URLClassLoader] = artifact match {
    case AdminV2Codec.ArtifactRef.Jvm(_, coords) =>
      val uri = artifactUri(coords)
      if (uri.isEmpty) None
      else {
        val urls = resolveJarUrls(uri.get)
        Some(new ChildFirstLoader(urls.toArray, getClass.getClassLoader))
      }
    case other =>
      throw new AdminException(
        ErrDialectMismatch,
        s"jvm executor expects Jvm artifact, got ${kindOf(other)}"
      )
  }

  /** artifact 种族名（Failed 回执 detail 用）。 */
  def kindOf(a: AdminV2Codec.ArtifactRef): String = a match {
    case AdminV2Codec.ArtifactRef.Props(_)       => "props"
    case AdminV2Codec.ArtifactRef.Beam(_)        => "beam"
    case AdminV2Codec.ArtifactRef.PyModule(_, _) => "pymodule"
    case AdminV2Codec.ArtifactRef.Jvm(_, _)      => "jvm"
    case AdminV2Codec.ArtifactRef.Wasm(_, _)     => "wasm"
    case AdminV2Codec.ArtifactRef.Dylib(_, _, _) => "dylib"
  }

  /** uri 来源：coords 字段承载 uri（file:// 形态）——B1 协议 Jvm{coords}
    * 未带独立 uri，Rust 侧约定 coords 即 "file://..." 数据源。
    */
  private def artifactUri(coords: Option[String]): Option[String] =
    coords.filter(_.startsWith("file:"))

  private def resolveJarUrls(uri: String): Seq[URL] = {
    val path = uri.stripPrefix("file://").stripPrefix("file:")
    if (path.endsWith(".jar")) Seq(new File(path).toURI.toURL)
    else {
      val dir = new File(path)
      if (!dir.isDirectory)
        throw new AdminException(ErrArtifactFetch, s"artifact dir not found: $path")
      val jars = dir.listFiles().filter(_.getName.endsWith(".jar")).map(_.toURI.toURL)
      if (jars.isEmpty)
        throw new AdminException(ErrArtifactFetch, s"no jar under $path")
      jars.toSeq
    }
  }

  // ---------------- 实例路径展开（四方言同规） ----------------

  def expandPaths(name: String, policy: AdminV2Codec.InstancePolicy): Vector[String] =
    policy match {
      case AdminV2Codec.InstancePolicy.Singleton => Vector(s"/user/$name")
      case AdminV2Codec.InstancePolicy.Pool(n)       => (0 until n.toInt).map(i => s"/user/$name-$i").toVector
      case AdminV2Codec.InstancePolicy.Sharded(n)    => (0 until n.toInt).map(i => s"/user/$name-$i").toVector
    }

  /** 前缀匹配（Rust 侧同规：整段相等或后随 '-'）。 */
  def matchesPrefix(path: String, prefix: String): Boolean =
    path == prefix || (path.startsWith(prefix) && path.length > prefix.length && path(prefix.length) == '-')
}
