package parrot.protocol.jvm

import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorRef, ActorSystem}
import AdminV2Codec._
import AdminV2Codec.AdminCommandV2._
import AdminV2Codec.AdminReplyV2._
import AdminV2Executor._
import org.scalatest.BeforeAndAfterAll
import org.scalatest.funsuite.AnyFunSuite

import java.io.File
import java.net.Socket
import java.nio.file.{Files, Path}
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.jar.{JarEntry, JarOutputStream}
import javax.tools.ToolProvider
import scala.collection.mutable
import scala.concurrent.Await
import scala.concurrent.duration._
import scala.util.Try

/** B5（DEV_09）：admin-v2 JVM 方言全套断言。
  *
  * 覆盖：
  *  1. 冻结向量逐字节对齐（docs/vectors/admin_v2.json 六条）
  *  2. varint 边界（250/251/65535/65536/2^32）
  *  3. 编解码 roundtrip 全形态 + 解码防御
  *  4. AdminPort 四命令生命周期（deploy/status/drain/stop + 错误分支 + 前缀规则）
  *  5. child-first loader 隔离：两版本同名类并存（不同 magic 辨识值）
  *  6. SYSTEM_EVENT 帧级全链（Netty 回环）—— MG11 jvm 侧
  */
class BridgeAdminV2Spec extends AnyFunSuite with BeforeAndAfterAll {

  private var sys: ActorSystem[AdminPort.Msg] = _

  override def beforeAll(): Unit = {
    sys = ActorSystem(Behaviors.empty[AdminPort.Msg], "bridge-admin-v2-test")
  }

  override def afterAll(): Unit = {
    if (sys != null) sys.terminate()
  }

  private def deadlineLoop(cond: () => Boolean, timeoutMs: Long = 10000): Unit = {
    val dl = System.nanoTime() + timeoutMs * 1000000L
    while (!cond() && System.nanoTime() < dl) Thread.sleep(5)
    assert(cond(), "deadline exceeded")
  }

  private def askAdmin(port: ActorRef[AdminPort.Msg], payload: Array[Byte]): AdminReplyV2 = {
    val q = new ConcurrentLinkedQueue[Array[Byte]]()
    port ! AdminPort.CmdIn(1L, "/_admin", payload, (_, _, out) => q.add(out))
    deadlineLoop(() => !q.isEmpty)
    decodeReply(q.poll())
  }

  // ---------------- 1. 冻结向量 ----------------

  private val frozenVectors = List(
    ("v2-deploy-props-singleton", "030001046563686f05312e302e3000086170702e6563686f0000"),
    ("v2-drain", "030102092f757365722f696478fb8813"),
    ("v2-stop", "030203092f757365722f696478"),
    ("v2-status", "030304062f757365722f"),
    ("v2-reply-deployed", "040001010a2f757365722f6563686f"),
    ("v2-reply-failed", "040409fb020a0b6e6f206578656375746f72")
  )

  frozenVectors.foreach { case (name, hex) =>
    test(s"frozen vector byte-exact: $name") {
      val bytes = hexToBytes(hex)
      if ((bytes(0) & 0xFF) == TagAdminCmdV2) {
        assert(hexOf(encodeCommand(decodeCommand(bytes))) == hex)
      } else {
        assert(hexOf(encodeReply(decodeReply(bytes))) == hex)
      }
    }
  }

  test("frozen vector semantic values") {
    decodeCommand(hexToBytes("030001046563686f05312e302e3000086170702e6563686f0000")) match {
      case Deploy(1, ComponentDeploy("echo", "1.0.0", ArtifactRef.Props("app.echo"),
        InstancePolicy.Singleton, None)) => // ok
      case other => fail(s"unexpected: $other")
    }
    decodeCommand(hexToBytes("030102092f757365722f696478fb8813")) match {
      case Drain(2, "/user/idx", 5000) => // ok
      case other => fail(s"unexpected: $other")
    }
    decodeCommand(hexToBytes("030203092f757365722f696478")) match {
      case Stop(3, "/user/idx") => // ok
      case other => fail(s"unexpected: $other")
    }
    decodeCommand(hexToBytes("030304062f757365722f")) match {
      case Status(4, "/user/") => // ok
      case other => fail(s"unexpected: $other")
    }
    decodeReply(hexToBytes("040001010a2f757365722f6563686f")) match {
      case Deployed(1, Vector("/user/echo")) => // ok
      case other => fail(s"unexpected: $other")
    }
    decodeReply(hexToBytes("040409fb020a0b6e6f206578656375746f72")) match {
      case Failed(9, 0x0A02, "no executor") => // ok（code=2562 DialectMismatch）
      case other => fail(s"unexpected: $other")
    }
  }

  // ---------------- 2. varint 边界 ----------------

  test("varint boundaries via config length 250/251/65535/65536") {
    List(250, 251, 65535, 65536).foreach { n =>
      val cmd = Deploy(7, ComponentDeploy("c", "1", ArtifactRef.Props("f"),
        InstancePolicy.Singleton, Some(Array.fill(n)(0x42.toByte))))
      decodeCommand(encodeCommand(cmd)) match {
        case Deploy(7, ComponentDeploy("c", "1", _, _, cfg)) =>
          assert(cfg.get.length == n)
        case other => fail(s"unexpected: $other")
      }
    }
    // req_id 2^32 边界（0xFD u64 档）
    decodeCommand(encodeCommand(Stop(1L << 32, "/user/x"))) match {
      case Stop(rid, "/user/x") => assert(rid == 1L << 32)
      case other => fail(s"unexpected: $other")
    }
  }

  test("varint prefix widths at 250/251 boundary") {
    // Status 布局：[tag=3][variant=3][req_id=1][varint strlen][str]
    // 偏移 3 即 strlen varint 首字节
    assert(statusCmd("a" * 250)(3) == 250.toByte)
    val b251 = statusCmd("a" * 251)
    assert((b251(3) & 0xFF) == 0xFB && (b251(4) & 0xFF) == 251 && b251(5) == 0)
    val b65536 = statusCmd("a" * 65536)
    assert((b65536(3) & 0xFF) == 0xFC)
  }

  private def statusCmd(s: String): Array[Byte] = encodeCommand(Status(1, s))

  // ---------------- 3. roundtrip 全形态 + 防御 ----------------

  test("roundtrip all artifact variants") {
    val arts = List(
      ArtifactRef.Props("app.echo"),
      ArtifactRef.Beam("crawler"),
      ArtifactRef.PyModule("jobs.crawl", Some("pip = [\"a\"]")),
      ArtifactRef.PyModule("jobs.crawl", None),
      ArtifactRef.Jvm("parrot.demo.Main", Some("file:///tmp/x.jar")),
      ArtifactRef.Jvm("parrot.demo.Main", None),
      ArtifactRef.Wasm("sha256:aa", "file:///tmp/a.wasm"),
      ArtifactRef.Dylib("sha256:bb", "file:///tmp/a.so", 1)
    )
    arts.foreach { a =>
      val cmd = Deploy(3, ComponentDeploy("n", "2.0.0", a, InstancePolicy.Pool(3),
        Some(Array(1, 2, 3))))
      decodeCommand(encodeCommand(cmd)) match {
        case Deploy(3, ComponentDeploy("n", "2.0.0", art, InstancePolicy.Pool(3), Some(cfg))) =>
          assert(art == a, s"artifact mismatch: $art != $a")
          assert(java.util.Arrays.equals(cfg, Array[Byte](1, 2, 3)))
        case other => fail(s"unexpected: $other")
      }
    }
  }

  test("roundtrip all reply variants") {
    val replies = List[AdminReplyV2](
      Deployed(1, Vector("/user/a", "/user/a-1")),
      Drained(2, 3, 4),
      Stopped(5),
      StatusReply(6, Vector(ComponentState("/user/a", "running", "1.0.0"))),
      Failed(7, 0x0A06, "spawn failed")
    )
    replies.foreach(r => assert(decodeReply(encodeReply(r)) == r, s"mismatch: $r"))
  }

  test("decode defenses: bad tag / truncated / trailing / bad variant / empty") {
    assert(Try(decodeCommand(Array(0x05.toByte, 0x00))).isFailure)
    assert(Try(decodeCommand(Array.emptyByteArray)).isFailure)
    assert(Try(decodeCommand(hexToBytes("030102092f75"))).isFailure) // 截断
    assert(Try(decodeCommand(hexToBytes("030203092f757365722f696478") :+ 0x00.toByte)).isFailure) // 尾随
    assert(Try(decodeCommand(Array(0x03.toByte, 0x09))).isFailure) // 未知变体
    assert(Try(decodeReply(Array(0x03.toByte, 0x00))).isFailure) // reply 载 cmd tag
    assert(Try(decodeCommand(Array(0x03.toByte, 0x00, 0x00, 0x01, 0x61.toByte))).isFailure) // Deploy req_id 后截断
    assert(Try(decodeCommand(Array(0x03.toByte, 0x00, 0x01, 0x00, 0x02, 0x61.toByte))).isFailure) // str len 越界
  }

  // ---------------- 4. AdminPort 生命周期 ----------------

  test("admin port deploy/status/stop lifecycle") {
    val port = sys.systemActorOf(AdminPort(sys), s"admin-lc-${System.nanoTime()}")
    askAdmin(port, encodeCommand(Deploy(11, ComponentDeploy(
      "echo", "1.0.0", ArtifactRef.Jvm(classOf[BridgeAdminV2Spec.LocalStubSpi].getName, None),
      InstancePolicy.Singleton, None)))) match {
      case Deployed(11, Vector("/user/echo")) => // ok
      case other => fail(s"unexpected: $other")
    }
    askAdmin(port, encodeCommand(Status(12, "/user/echo"))) match {
      case StatusReply(12, Vector(ComponentState("/user/echo", "running", "1.0.0"))) => // ok
      case other => fail(s"unexpected: $other")
    }
    askAdmin(port, encodeCommand(Stop(13, "/user/echo"))) match {
      case Stopped(13) => // ok
      case other => fail(s"unexpected: $other")
    }
    askAdmin(port, encodeCommand(Status(14, "/user/echo"))) match {
      case Failed(14, ErrComponentNotFound, _) => // ok
      case other => fail(s"unexpected: $other")
    }
  }

  test("admin port drain fast and timeout") {
    val port = sys.systemActorOf(AdminPort(sys), s"admin-dr-${System.nanoTime()}")
    askAdmin(port, encodeCommand(Deploy(21, ComponentDeploy(
      "fast", "1.0.0", ArtifactRef.Jvm(classOf[BridgeAdminV2Spec.LocalStubSpi].getName, None),
      InstancePolicy.Singleton, None)))) match {
      case Deployed(21, _) => // ok
      case other => fail(s"unexpected: $other")
    }
    askAdmin(port, encodeCommand(Drain(22, "/user/fast", 2000))) match {
      case Drained(22, 1, 0) => // ok
      case other => fail(s"unexpected: $other")
    }
    // 慢 drain：钩子 300ms > 50ms deadline → aborted=1（实例保留可 stop 清）
    askAdmin(port, encodeCommand(Deploy(23, ComponentDeploy(
      "slow", "1.0.0", ArtifactRef.Jvm(classOf[BridgeAdminV2Spec.LocalSlowSpi].getName, None),
      InstancePolicy.Singleton, None)))) match {
      case Deployed(23, _) => // ok
      case other => fail(s"unexpected: $other")
    }
    askAdmin(port, encodeCommand(Drain(24, "/user/slow", 50))) match {
      case Drained(24, 0, 1) => // ok
      case other => fail(s"unexpected: $other")
    }
    askAdmin(port, encodeCommand(Stop(25, "/user/slow"))) match {
      case Stopped(25) => // ok
      case other => fail(s"unexpected: $other")
    }
  }

  test("admin port prefix matching '-' rule") {
    val port = sys.systemActorOf(AdminPort(sys), s"admin-pm-${System.nanoTime()}")
    List("idx", "idx2").foreach { n =>
      askAdmin(port, encodeCommand(Deploy(31, ComponentDeploy(
        n, "1.0.0", ArtifactRef.Jvm(classOf[BridgeAdminV2Spec.LocalStubSpi].getName, None),
        InstancePolicy.Pool(2), None)))) match {
        case Deployed(31, _) => // ok
        case other => fail(s"unexpected: $other")
      }
    }
    // /user/idx 命中 idx + idx-0/1；不命中 idx2-*
    askAdmin(port, encodeCommand(Stop(32, "/user/idx"))) match {
      case Stopped(32) => // ok
      case other => fail(s"unexpected: $other")
    }
    askAdmin(port, encodeCommand(Status(33, "/user/idx2"))) match {
      case StatusReply(33, states) => assert(states.length == 2)
      case other => fail(s"unexpected: $other")
    }
    // 未部署前缀 → 0x0A03
    askAdmin(port, encodeCommand(Stop(34, "/user/nope"))) match {
      case Failed(34, ErrComponentNotFound, _) => // ok
      case other => fail(s"unexpected: $other")
    }
  }

  test("admin port dialect mismatch on non-jvm artifact") {
    val port = sys.systemActorOf(AdminPort(sys), s"admin-dm-${System.nanoTime()}")
    askAdmin(port, encodeCommand(Deploy(41, ComponentDeploy(
      "py", "1.0.0", ArtifactRef.PyModule("m", None), InstancePolicy.Singleton, None)))) match {
      case Failed(41, ErrDialectMismatch, _) => // ok
      case other => fail(s"unexpected: $other")
    }
  }

  test("admin port spawn failure rolls back and reports 0x0A06") {
    val port = sys.systemActorOf(AdminPort(sys), s"admin-sf-${System.nanoTime()}")
    // 非 ComponentSpi 的既有类 → ErrSpawnFailed
    askAdmin(port, encodeCommand(Deploy(42, ComponentDeploy(
      "bad", "1.0.0", ArtifactRef.Jvm("java.lang.String", None),
      InstancePolicy.Singleton, None)))) match {
      case Failed(42, ErrSpawnFailed, _) => // ok
      case other => fail(s"unexpected: $other")
    }
    // 不存在的类 → 0x0A06（loader None 路径的 Class.forName 失败）
    askAdmin(port, encodeCommand(Deploy(43, ComponentDeploy(
      "bad2", "1.0.0", ArtifactRef.Jvm("parrot.no.SuchClass", None),
      InstancePolicy.Singleton, None)))) match {
      case Failed(43, ErrSpawnFailed, _) => // ok
      case other => fail(s"unexpected: $other")
    }
    // 登记表干净（失败不留残）
    askAdmin(port, encodeCommand(Status(44, "/user/"))) match {
      case Failed(44, ErrComponentNotFound, _) => // ok
      case other => fail(s"unexpected: $other")
    }
  }

  // ---------------- 5. child-first loader 隔离 ----------------

  test("child-first loader: two versions of same class name coexist") {
    val dir = Files.createTempDirectory("parrot-jvm-admin-v2")
    try {
      val v1 = compileSpiJar(dir, "v1", 111)
      val v2 = compileSpiJar(dir, "v2", 222)
      assert(v1.exists() && v2.exists())

      val l1 = buildLoader(ArtifactRef.Jvm("parrot.demo.DemoSpi", Some(v1.toURI.toString)))
      val l2 = buildLoader(ArtifactRef.Jvm("parrot.demo.DemoSpi", Some(v2.toURI.toString)))
      assert(l1.isDefined && l2.isDefined)
      val c1 = Class.forName("parrot.demo.DemoSpi", true, l1.get)
      val c2 = Class.forName("parrot.demo.DemoSpi", true, l2.get)
      assert(c1 ne c2, "同名类经两个 child loader 必须是不同 Class 实例")
      assert(magicOf(c1) == 111 && magicOf(c2) == 222)

      // AdminPort：不同名组件并存 = 两版本类同时活跃（热升级基础）
      val port = sys.systemActorOf(AdminPort(sys), s"admin-cf-${System.nanoTime()}")
      askAdmin(port, encodeCommand(Deploy(51, ComponentDeploy(
        "demoA", "1.0.0", ArtifactRef.Jvm("parrot.demo.DemoSpi", Some(v1.toURI.toString)),
        InstancePolicy.Singleton, None)))) match {
        case Deployed(51, Vector("/user/demoA")) => // ok
        case other => fail(s"unexpected: $other")
      }
      askAdmin(port, encodeCommand(Deploy(52, ComponentDeploy(
        "demoB", "2.0.0", ArtifactRef.Jvm("parrot.demo.DemoSpi", Some(v2.toURI.toString)),
        InstancePolicy.Singleton, None)))) match {
        case Deployed(52, Vector("/user/demoB")) => // ok
        case other => fail(s"unexpected: $other")
      }
      askAdmin(port, encodeCommand(Status(53, "/user/demoA"))) match {
        case StatusReply(53, Vector(ComponentState("/user/demoA", "running", "1.0.0"))) => // ok
        case other => fail(s"unexpected: $other")
      }
      // redeploy 同名 → 原子替换（v1 → v2）
      askAdmin(port, encodeCommand(Deploy(55, ComponentDeploy(
        "demoA", "2.0.0", ArtifactRef.Jvm("parrot.demo.DemoSpi", Some(v2.toURI.toString)),
        InstancePolicy.Singleton, None)))) match {
        case Deployed(55, _) => // ok
        case other => fail(s"unexpected: $other")
      }
      askAdmin(port, encodeCommand(Status(56, "/user/demoA"))) match {
        case StatusReply(56, Vector(ComponentState("/user/demoA", "running", "2.0.0"))) => // ok
        case other => fail(s"unexpected: $other")
      }
    } finally deleteRecursively(dir)
  }

  test("child loader parent-first for protocol/akka classes") {
    val dir = Files.createTempDirectory("parrot-jvm-admin-v2-pf")
    try {
      val jar = compileSpiJar(dir, "pf", 555)
      val l = buildLoader(ArtifactRef.Jvm("parrot.demo.DemoSpi", Some(jar.toURI.toString)))
      assert(l.isDefined)
      // 协议类（parrot.protocol.jvm. 前缀）必须来自父链——跨加载器类型身份
      assert(l.get.loadClass(classOf[ComponentSpi].getName) eq classOf[ComponentSpi])
      assert(l.get.loadClass(classOf[ComponentContext].getName) eq classOf[ComponentContext])
      assert(l.get.loadClass("akka.actor.typed.Behavior") eq classOf[akka.actor.typed.Behavior[_]])
      // artifact 自带类 child-first
      val demo = l.get.loadClass("parrot.demo.DemoSpi")
      assert(magicOf(demo) == 555)
    } finally deleteRecursively(dir)
  }

  test("buildLoader dialect mismatch and bad uri") {
    assert(Try(buildLoader(ArtifactRef.Props("f"))).isFailure) // 0x0A02
    Try(buildLoader(ArtifactRef.Props("f"))).failed.get match {
      case e: AdminException => assert(e.code == ErrDialectMismatch)
      case other => fail(s"unexpected: $other")
    }
    val badDir = Files.createTempDirectory("parrot-empty")
    try {
      // 空目录 → 0x0A00（no jar）
      Try(buildLoader(ArtifactRef.Jvm("x.Some", Some(badDir.toUri.toString)))).failed.get match {
        case e: AdminException => assert(e.code == ErrArtifactFetch)
        case other => fail(s"unexpected: $other")
      }
    } finally deleteRecursively(badDir)
  }

  // ---------------- 6. 帧级全链（Netty 回环）MG11 jvm 侧 ----------------

  test("frame-level admin v2 over netty loopback") {
    val gw = new GatewayLoopback()
    try {
      gw.start()
      deadlineLoop(() => gw.port > 0)
      val conn = gw.connect()
      try {
        conn.send(WireFrame.Frame(1, WireFrame.FrameType.HANDSHAKE, 0, 0, 0, 8, "",
          "__handshake__", WireFrame.handshakeBody("b5-test")))
        assert(conn.expect(_.frameType == WireFrame.FrameType.HANDSHAKE_ACK) != null)

        val deployCmd = Deploy(61, ComponentDeploy(
          "netecho", "1.0.0", ArtifactRef.Jvm(classOf[BridgeAdminV2Spec.LocalStubSpi].getName, None),
          InstancePolicy.Pool(2), None))
        conn.send(WireFrame.Frame(1, WireFrame.FrameType.SYSTEM_EVENT, 0, 61, 0, 8,
          "parrot://b5-test/_admin", "", encodeCommand(deployCmd)))
        conn.expectAdminReply(61) match {
          case Deployed(61, Vector("/user/netecho-0", "/user/netecho-1")) => // ok
          case other => fail(s"unexpected: $other")
        }

        // 非 admin tag SYSTEM_EVENT → 吞帧不崩（gossip 前向兼容）
        conn.send(WireFrame.Frame(1, WireFrame.FrameType.SYSTEM_EVENT, 0, 99, 0, 8, "", "",
          Array(0x05.toByte, 0x01.toByte)))

        conn.send(WireFrame.Frame(1, WireFrame.FrameType.SYSTEM_EVENT, 0, 62, 0, 8,
          "parrot://b5-test/_admin", "", encodeCommand(Status(62, "/user/netecho"))))
        conn.expectAdminReply(62) match {
          case StatusReply(62, states) => assert(states.length == 2)
          case other => fail(s"unexpected: $other")
        }

        conn.send(WireFrame.Frame(1, WireFrame.FrameType.SYSTEM_EVENT, 0, 63, 0, 8,
          "parrot://b5-test/_admin", "", encodeCommand(Stop(63, "/user/netecho"))))
        conn.expectAdminReply(63) match {
          case Stopped(63) => // ok
          case other => fail(s"unexpected: $other")
        }
      } finally conn.close()
    } finally gw.shutdown()
  }

  // ---------------- 本地桩 SPI ----------------
  // 定义在伴生 object BridgeAdminV2Spec（静态——反射无参构造要求）

  // ---------------- Java 源动态编译 fixture jar ----------------

  /** 生成实现 ComponentSpi 的同名类 jar（v magic 辨识）。
    * Scala trait 在字节码 = 接口（具体方法 default）——Java 类可直接
    * implements；javac 由 JDK ToolProvider 提供（测试 JVM 即 JDK）。
    */
  private def compileSpiJar(dir: Path, tag: String, magic: Int): File = {
    val sub = Files.createDirectories(dir.resolve(s"src-$tag"))
    val src = sub.resolve("DemoSpi.java") // javac 公共类名 = 文件名
    val source = s"""package parrot.demo;
                    |public class DemoSpi implements parrot.protocol.jvm.ComponentSpi {
                    |    public static final int MAGIC = $magic;
                    |    public akka.actor.typed.Behavior behavior(parrot.protocol.jvm.ComponentContext ctx) {
                    |        return akka.actor.typed.javadsl.Behaviors.empty();
                    |    }
                    |    public int magic() { return MAGIC; }
                    |}
                    |""".stripMargin
    Files.write(src, source.getBytes(java.nio.charset.StandardCharsets.UTF_8))
    val compiler = ToolProvider.getSystemJavaCompiler
    assert(compiler != null, "system java compiler unavailable (need JDK)")
    val outDir = Files.createDirectories(dir.resolve(s"classes-$tag"))
    val cp = System.getProperty("java.class.path")
    val fm = compiler.getStandardFileManager(null, null, null)
    val task = compiler.getTask(null, null, null,
      java.util.Arrays.asList("-classpath", cp, "-d", outDir.toString),
      null, fm.getJavaFileObjectsFromFiles(java.util.Collections.singletonList(src.toFile)))
    assert(task.call(), s"fixture compile failed for $tag")

    val jar = dir.resolve(s"demo-$tag.jar").toFile
    val jos = new JarOutputStream(java.nio.file.Files.newOutputStream(jar.toPath))
    try {
      val it = Files.walk(outDir).filter(_.toString.endsWith(".class")).sorted().iterator()
      while (it.hasNext) {
        val cls = it.next()
        val entry = new JarEntry(outDir.relativize(cls).toString)
        jos.putNextEntry(entry)
        jos.write(Files.readAllBytes(cls))
        jos.closeEntry()
      }
    } finally jos.close()
    jar
  }

  private def magicOf(cls: Class[_]): Int =
    cls.getMethod("magic").invoke(cls.getDeclaredConstructor().newInstance()).asInstanceOf[Integer]

  // ---------------- Netty 回环网关 ----------------

  private final class GatewayLoopback {
    @volatile var port: Int = -1
    private var ext: ParrotTransportExtension = _
    private var gwSys: ActorSystem[_] = _

    def start(): Unit = {
      gwSys = ActorSystem(Behaviors.setup[Any] { ctx =>
        val bridge = ctx.spawn(BridgeActor(_ => None), "bridge")
        val admin = ctx.spawn(AdminPort(ctx.system), "admin-port")
        ParrotServerHandler.initSystem(ctx.system)
        ext = new ParrotTransportExtension(ctx.system, bridge, "b5-gw", Some(admin))
        ext.listen(0)
        port = ext.port
        Behaviors.empty
      }, s"b5-gw-${System.nanoTime()}")
    }

    def connect(): Conn = new Conn(new Socket("127.0.0.1", port))

    def shutdown(): Unit = {
      if (ext != null) ext.shutdown()
      if (gwSys != null) gwSys.terminate()
    }
  }

  private final class Conn(socket: Socket) {
    private val out = socket.getOutputStream
    private val in = socket.getInputStream
    private val frames = new ConcurrentLinkedQueue[WireFrame.Frame]()
    private val dec = new FrameAccumulator
    private val thread = new Thread(() => {
      val buf = new Array[Byte](65536)
      try {
        var n = in.read(buf)
        while (n >= 0) {
          dec.feed(java.util.Arrays.copyOf(buf, n)).foreach(frames.add)
          n = in.read(buf)
        }
      } catch { case _: java.io.IOException => () }
    }, "b5-conn-reader")
    thread.setDaemon(true)
    thread.start()

    def send(f: WireFrame.Frame): Unit = {
      out.write(f.encode())
      out.flush()
    }

    def expect(pred: WireFrame.Frame => Boolean, ms: Long = 5000): WireFrame.Frame = {
      val dl = System.nanoTime() + ms * 1000000L
      var f = poll(pred)
      while (f == null && System.nanoTime() < dl) { Thread.sleep(5); f = poll(pred) }
      f
    }

    private def poll(pred: WireFrame.Frame => Boolean): WireFrame.Frame = {
      val it = frames.iterator()
      while (it.hasNext) {
        val f = it.next()
        if (pred(f)) { frames.remove(f); return f }
      }
      null
    }

    def expectAdminReply(reqId: Long, ms: Long = 5000): AdminReplyV2 = {
      val f = expect(fr => fr.frameType == WireFrame.FrameType.SYSTEM_EVENT &&
        fr.payload.nonEmpty && (fr.payload(0) & 0xFF) == TagAdminReplyV2 &&
        Try(decodeReply(fr.payload)).toOption.exists(_.reqId == reqId), ms)
      assert(f != null, s"admin reply $reqId not received")
      decodeReply(f.payload)
    }

    def close(): Unit = socket.close()
  }

  /** 简单帧累积器（WireFrame.decode 半包处理）。 */
  private final class FrameAccumulator {
    private var buf = Array.emptyByteArray
    def feed(chunk: Array[Byte]): Seq[WireFrame.Frame] = {
      buf = if (buf.isEmpty) chunk else buf ++ chunk
      val out = mutable.ArrayBuffer.empty[WireFrame.Frame]
      var continue = true
      while (continue) {
        WireFrame.decode(buf) match {
          case Some((f, consumed)) =>
            out += f
            buf = java.util.Arrays.copyOfRange(buf, consumed, buf.length)
          case None => continue = false
        }
      }
      out.toSeq
    }
  }

  // ---------------- misc ----------------

  private def hexToBytes(hex: String): Array[Byte] =
    hex.sliding(2, 2).map(Integer.parseInt(_, 16).toByte).toArray

  private def hexOf(b: Array[Byte]): String =
    b.map(v => f"${v & 0xFF}%02x").mkString

  private def deleteRecursively(p: Path): Unit = {
    if (Files.isDirectory(p)) Files.list(p).forEach(deleteRecursively)
    Files.deleteIfExists(p)
    ()
  }
}

object BridgeAdminV2Spec {
  // 父链直载桩：置于伴生 object（静态）——反射 getDeclaredConstructor()
  // 需无参构造；spec 实例内部类隐式持外部引用会致 spawn 失败。
  class LocalStubSpi extends ComponentSpi {
    override def behavior(ctx: ComponentContext): akka.actor.typed.Behavior[Any] =
      Behaviors.receiveMessage[Any] {
        case BridgeActor.BridgeAsk(_, payload, replyTo) =>
          replyTo ! BridgeActor.BridgeReplyOk("bin:jvm.test.LocalStub#v1", payload)
          Behaviors.same
        case _ => Behaviors.same
      }
  }

  /** 慢 drain 桩：parrotDrain 300ms 后完成（超时测试用）。 */
  class LocalSlowSpi extends LocalStubSpi {
    override def parrotDrain(): java.util.concurrent.CompletableFuture[Unit] = {
      val f = new java.util.concurrent.CompletableFuture[Unit]()
      new Thread(() => { Thread.sleep(300); f.complete(()) }, "drain-sim").start()
      f
    }
  }
}
