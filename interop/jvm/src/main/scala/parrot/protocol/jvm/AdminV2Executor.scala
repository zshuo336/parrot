package parrot.protocol.jvm

import java.io.File
import java.net.{URI, URL, URLClassLoader}
import java.nio.charset.StandardCharsets.UTF_8
import java.security.MessageDigest
import java.util.concurrent.atomic.AtomicBoolean
import scala.util.{Failure, Success, Try}

/** B5（DEV_09）：admin-v2 四命令的 akka/JVM 方言执行器。
  *
  * - DeployComponent{Jvm} → child-first `URLClassLoader`（jar 目录/坐标）
  *   → `loader.loadClass(main_class)` 反射构造 `ComponentSpi` →
  *   management-owned child subtree（逻辑路径 /user/{name}[-{i}]）。
  * - Drain → 实例 drain 钩子 + 共享 deadline，超时 → aborted（摘路由）。
  * - Stop → 退休匹配路径的实例子树，超时保留 retiring 句柄。
  * - Status → 登记表状态，由实例退休通知收敛；不是独立健康探针。
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

  /** Component registry entry owned by one AdminPort. */
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
      spi: Option[ComponentSpi],
      ref: akka.actor.typed.ActorRef[Any],
      stopper: Long => Unit,
      drainer: Long => Unit,
      alive: AtomicBoolean,
      state: String = "running"
  )

  /** R4（应用体系架构纠正）：deploy 实例 → Bridge 路由注册表。
    *
    * AdminPort spawn 实例后登记（path → ask 中转 actor）；stop/drain
    * 清除。网关 BridgeActor resolve 先查本表——已部署组件优先于内置
    * 探针 targets（业务代码已迁各 app 的 jvm 目录，网关只留协议骨架）。
    *
    * ask 中转：BridgeAsk/BridgeReplyOk 的类型身份在网关 classloader——
    * child loader 里的组件 Behavior[Any] 无法直接消费，故经适配 actor
    * 转发（payload 透传——协议键与字节双方言无身份耦合）。
    */
  /** The default route scope belongs to one ActorSystem. Independent gateways
    * within that system can explicitly create and share their own scope.
    */
  object ComponentRoutes extends akka.actor.typed.ExtensionId[ComponentRoutes] {
    override def createExtension(system: akka.actor.typed.ActorSystem[_]): ComponentRoutes =
      owned(system, new Object)

    def newScope(system: akka.actor.typed.ActorSystem[_]): ComponentRoutes =
      owned(system, apply(system).owner)

    private def owned(system: akka.actor.typed.ActorSystem[_], owner: Object): ComponentRoutes = {
      val scope = new ComponentRoutes(owner)
      scope.bindOwner(SystemRetirement(system).register(() => scope.close()))
      scope
    }
  }

  final class ComponentRoutes private (private val owner: Object) extends akka.actor.typed.Extension {
    private final case class Route(ref: akka.actor.typed.ActorRef[Any], generation: Object = new Object)
    @volatile private var routes = Map.empty[String, Route]
    private var closed = false
    private var retirement: Option[SystemRetirement#Registration] = None
    private def bindOwner(registration: SystemRetirement#Registration): Unit = synchronized {
      if (closed) registration.close() else retirement = Some(registration)
    }

    private[jvm] def belongsTo(system: akka.actor.typed.ActorSystem[_]): Boolean =
      owner eq ComponentRoutes(system).owner

    /** Same-owner registration is idempotent; a different owner cannot be overwritten. */
    def put(path: String, ref: akka.actor.typed.ActorRef[Any]): Boolean = synchronized {
      if (closed || routes.get(path).exists(_.ref != ref)) false
      else { if (!routes.contains(path)) routes = routes.updated(path, Route(ref)); true }
    }
    /** Publish an entire ready cohort at once; never overwrite another owner. */
    def publish(entries: Vector[(String, akka.actor.typed.ActorRef[Any], AtomicBoolean)]): Boolean = synchronized {
      if (closed || entries.map(_._1).distinct.size != entries.size ||
          entries.exists(e => !e._3.get() || routes.contains(e._1))) false
      else { routes = routes ++ entries.map(e => e._1 -> Route(e._2)); true }
    }
    def remove(path: String): Unit = synchronized { routes = routes - path }
    def removeOwned(path: String, ref: akka.actor.typed.ActorRef[Any]): Unit = synchronized {
      if (routes.get(path).exists(_.ref == ref)) routes = routes - path
    }
    /** Retirement and publication share the same lock, including pre-publication death. */
    def retired(path: String, ref: akka.actor.typed.ActorRef[Any], alive: AtomicBoolean): Unit = synchronized {
      alive.set(false)
      removeOwned(path, ref)
    }
    def close(): Unit = synchronized {
      closed = true; routes = Map.empty
      retirement.foreach(_.close()); retirement = None
    }
    def get(path: String): Option[akka.actor.typed.ActorRef[Any]] = routes.get(path).map(_.ref)

    /** Resolve only descendants of the currently published implementation actor.
      * Recheck the publication generation after async resolution, so retirement,
      * replacement and close fence lookups already in flight (including ABA).
      */
    def resolveChild(path: String)(implicit ec: scala.concurrent.ExecutionContext): scala.concurrent.Future[Option[akka.actor.typed.ActorRef[Any]]] = {
      import akka.actor.typed.scaladsl.adapter._
      import scala.concurrent.Future
      import scala.concurrent.duration._
      val parts = path.split("/", -1).toVector
      val valid = parts.length >= 2 && parts.forall(p =>
        p.nonEmpty && p != "." && p != ".." &&
          p.forall(c => c.isLetterOrDigit || c == '-' || c == '_' || c == '.'))
      if (!valid) Future.successful(None)
      else routes.get(parts.head) match {
        case None => Future.successful(None)
        case Some(root) =>
          akka.actor.ActorSelection(root.ref.toClassic, ("impl" +: parts.tail).mkString("/"))
            .resolveOne(2.seconds).map { ref => synchronized {
              if (routes.get(parts.head).contains(root)) Some(ref.toTyped[Any]) else None
            }}
            .recover { case _: akka.actor.ActorNotFound => None }
      }
    }
  }

  final class AdminException(val code: Int, msg: String) extends RuntimeException(msg)

  // ---------------- child-first loader ----------------

  /** child-first：artifact 自带类优先（子加载），父链类（akka/scala/JDK）
    * 委派——跨加载器类型身份保障（BridgeAsk 等协议类型不可 child 载入）。
    */
  private final class ChildFirstLoader(urls: Array[URL], parent: ClassLoader)
      extends URLClassLoader(urls, parent) {
    private val parentFirstPrefixes = Array(
      "java.", "javax.", "jdk.", "sun.", "scala.", "akka.", "com.typesafe.",
      "io.netty.", "io.opentelemetry.api.", "io.opentelemetry.context.",
      "org.slf4j.", "ch.qos.", "parrot.protocol.jvm."
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

  /** 解析 artifact uri → child loader（目录扫 .jar / 单 .jar）。
    * 非 Jvm artifact → DialectMismatch；显式 URI 无效时拒绝加载。
    */
  def buildLoader(
      artifact: AdminV2Codec.ArtifactRef
  ): Option[URLClassLoader] = artifact match {
    case jv @ AdminV2Codec.ArtifactRef.Jvm(_, _, _) =>
      val uri = artifactUri(jv)
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
    case AdminV2Codec.ArtifactRef.Beam(_, _)       => "beam"
    case AdminV2Codec.ArtifactRef.PyModule(_, _, _) => "pymodule"
    case AdminV2Codec.ArtifactRef.Jvm(_, _, _)     => "jvm"
    case AdminV2Codec.ArtifactRef.Wasm(_, _)     => "wasm"
    case AdminV2Codec.ArtifactRef.Dylib(_, _, _) => "dylib"
  }

  /** uri 来源（R3）：uri 字段（app 构建产物直发——file:// 形态）优先；
    * 缺省时 coords 承载 uri（file:// 前缀形态——B1 兼容）。
    */
  private def artifactUri(a: AdminV2Codec.ArtifactRef): Option[String] = a match {
    case AdminV2Codec.ArtifactRef.Jvm(_, coords, uri) =>
      // An explicit URI must be validated, never silently replaced by parent
      // classpath loading when its scheme is unsupported.
      uri.orElse(coords.filter(_.startsWith("file:")))
    case _ => None
  }

  private def resolveJarUrls(uri: String): Seq[URL] = {
    val file = try new File(new URI(uri))
    catch {
      case scala.util.control.NonFatal(_) =>
        throw new AdminException(ErrArtifactFetch, "artifact requires an absolute local file URI without authority, query or fragment")
    }
    if (file.getName.endsWith(".jar")) {
      if (!file.isFile)
        throw new AdminException(ErrArtifactFetch, s"artifact jar not found: $file")
      Seq(file.toURI.toURL)
    }
    else {
      if (!file.isDirectory)
        throw new AdminException(ErrArtifactFetch, s"artifact dir not found: $file")
      val entries = Option(file.listFiles()).getOrElse(
        throw new AdminException(ErrArtifactFetch, s"cannot list artifact dir: $file"))
      val jars = entries.filter(f => f.isFile && f.getName.endsWith(".jar"))
        .sortBy(_.getName).map(_.toURI.toURL)
      if (jars.isEmpty)
        throw new AdminException(ErrArtifactFetch, s"no jar under $file")
      jars.toSeq
    }
  }

  // ---------------- 实例路径展开（四方言同规） ----------------

  def expandPaths(name: String, policy: AdminV2Codec.InstancePolicy): Vector[String] =
    policy match {
      case AdminV2Codec.InstancePolicy.Singleton => Vector(s"/user/$name")
      case AdminV2Codec.InstancePolicy.Pool(n) => boundedPaths(name, n)
      case AdminV2Codec.InstancePolicy.Sharded(n) => boundedPaths(name, n)
    }

  private def boundedPaths(name: String, n: Long): Vector[String] = {
    if (n <= 0 || n > 256) throw new AdminException(ErrSpawnFailed, "component instance count must be in 1..256")
    (0 until n.toInt).map(i => s"/user/$name-$i").toVector
  }

  /** 前缀匹配（Rust 侧同规：整段相等或后随 '-'）。 */
  def matchesPrefix(path: String, prefix: String): Boolean =
    path == prefix || (path.startsWith(prefix) && path.length > prefix.length && path(prefix.length) == '-')
}
