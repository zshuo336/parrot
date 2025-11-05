package websearch.search

import akka.actor.typed.ActorSystem
import akka.actor.typed.scaladsl.AskPattern._
import parrot.protocol.jvm.ComponentContext
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk, BridgeReplyErr}
import java.io.{ByteArrayOutputStream, DataOutputStream, File}
import java.nio.{ByteBuffer, ByteOrder}
import java.nio.file.{Files, Path}
import scala.collection.mutable
import scala.concurrent.Await
import scala.concurrent.duration._
import scala.jdk.CollectionConverters._

object StorageContracts extends App {
  import SearchBehavior.{Doc, readSeg, writeSeg}
  var passed = 0; var failed = 0
  def i32(n: Int): Array[Byte] = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(n).array()
  def i64(n: Long): Array[Byte] = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(n).array()
  def str(t: String): Array[Byte] = { val b = t.getBytes("UTF-8"); i32(b.length) ++ b }
  def legacy(docs: Seq[(Long, Doc)], terms: Seq[(String, Seq[(Long, Int)])]): Array[Byte] = {
    val b = new ByteArrayOutputStream; val out = new DataOutputStream(b)
    out.writeInt(docs.size)
    docs.foreach { case (id, d) => out.writeLong(id); out.writeUTF(d.url); out.writeUTF(d.title); out.writeUTF(d.snippet) }
    out.writeInt(terms.size)
    terms.foreach { case (t, ds) => out.writeUTF(t); out.writeInt(ds.size); ds.foreach { case (id, f) => out.writeLong(id); out.writeInt(f) } }
    out.close(); b.toByteArray
  }
  def test(name: String)(run: Path => Unit): Unit = {
    val dir = Files.createTempDirectory("ws-storage-")
    try { run(dir); passed += 1; println("PASS storage " + name) }
    catch { case scala.util.control.NonFatal(e) => failed += 1; println(s"FAIL storage $name: $e"); e.printStackTrace() }
    finally { val all = Files.walk(dir); try all.iterator.asScala.toVector.reverse.foreach(Files.delete) finally all.close() }
  }
  def rejected(file: File, bytes: Array[Byte]): Unit = {
    Files.write(file.toPath, bytes)
    val result = scala.util.Try(readSeg(file))
    assert(result.isFailure, "corrupt segment accepted")
  }
  val doc = Doc("https://a.test", "alpha", "中文😀")
  test("legacy negative counts and trailing bytes rejected") { dir =>
    val f = dir.resolve("bad.segment").toFile
    rejected(f, Array.fill[Byte](8)(-1))
    rejected(f, legacy(Seq.empty, Seq.empty) ++ Array[Byte](0))
  }
  test("duplicate document term and posting identities rejected") { dir =>
    val f = dir.resolve("bad.segment").toFile
    rejected(f, legacy(Seq(1L -> doc, 1L -> doc), Seq.empty))
    rejected(f, legacy(Seq.empty, Seq("a" -> Seq.empty, "a" -> Seq.empty)))
    rejected(f, legacy(Seq.empty, Seq("a" -> Seq(1L -> 1, 1L -> 2))))
  }
  test("legacy valid bytes and every truncation") { dir =>
    val f = dir.resolve("legacy.segment").toFile
    val bytes = legacy(Seq(-1L -> doc), Seq("中文" -> Seq(-1L -> -1)))
    Files.write(f.toPath, bytes)
    val (d, p) = readSeg(f)
    assert(d(-1) == doc && p("中文")(-1) == -1)
    bytes.indices.foreach(n => rejected(f, bytes.take(n)))
  }
  test("long Unicode metadata and terms survive encoding") { dir =>
    val f = dir.resolve("long.segment").toFile
    val title = "中文😀" * 30000
    val term = "词" * 30000
    val docs = mutable.LongMap(1L -> doc.copy(title = title))
    val terms = mutable.Map(term -> mutable.LongMap(1L -> -1))
    writeSeg(f, docs, terms)
    val (d, p) = readSeg(f)
    assert(d(1).title == title && p(term)(1) == -1)
  }
  test("metadata-only Flush survives restart") { dir =>
    val s = new Session(dir)
    try {
      s.start(); s.putDoc("metadata-only"); s.ok("Flush"); s.stop(); s.start()
      assert(s.health.contains("\"docs\":1"), s.health)
    } finally s.stop()
  }
  test("latest full snapshot replaces superseded documents") { dir =>
    val index = Files.createDirectory(dir.resolve("index"))
    Files.write(index.resolve("seg-00000.segment"), legacy(Seq(1L -> doc), Seq("alpha" -> Seq(1L -> 1))))
    Files.write(index.resolve("seg-00001.segment"), legacy(Seq.empty, Seq.empty))
    val s = new Session(dir)
    try { s.start(); assert(s.health.contains("\"docs\":0"), s.health); assert(s.health.contains("\"terms\":0"), s.health) }
    finally s.stop()
  }
  test("gapped legacy generations cannot overwrite newer state on replay") { dir =>
    val index = Files.createDirectory(dir.resolve("index"))
    Files.write(index.resolve("seg-00009.segment"), legacy(Seq(1L -> doc), Seq("alpha" -> Seq(1L -> 1))))
    val s = new Session(dir)
    try {
      s.start(); s.putDoc("replacement"); s.ok("Flush"); s.stop(); s.start()
      assert(s.query.contains("replacement"), s.query)
    } finally s.stop()
  }
  test("one live index owner and release on actor termination") { dir =>
    val first = new Session(dir); val second = new Session(dir)
    try {
      first.start()
      assert(scala.util.Try(second.start()).isFailure, "same data directory admitted two live writers")
      first.stop(); second.start(); assert(second.health.contains("\"docs\":0"))
    } finally { first.stop(); second.stop() }
  }
  test("failed publication replies error and rejects further mutation") { dir =>
    val s = new Session(dir)
    val index = dir.resolve("index"); val moved = dir.resolve("saved-index")
    try {
      s.start(); s.putDoc("committed"); s.putTerm(); s.ok("Flush")
      Files.move(index, moved); Files.write(index, Array[Byte](0))
      try {
        assert(s.request("Flush").isInstanceOf[BridgeReplyErr], "storage failure did not reply error")
        assert(s.request("DocMeta", i64(1) ++ str("url") ++ str("uncommitted") ++ str("alpha")).isInstanceOf[BridgeReplyErr])
      } finally { s.stop(); Files.delete(index); Files.move(moved, index) }
      s.start(); assert(s.query.contains("committed") && !s.query.contains("uncommitted"))
    } finally s.stop()
  }
  test("Clear is committed before acknowledgment and survives restart") { dir =>
    val s = new Session(dir)
    try {
      s.start(); s.putDoc("old"); s.putTerm(); s.ok("Flush"); s.ok("Clear"); s.stop(); s.start()
      assert(s.query == "[]"); assert(s.health.contains("\"docs\":0"))
    } finally s.stop()
  }
  test("v2 independent vector checksum corruptions and every truncation") { dir =>
    def checksummed(content: Array[Byte]): Array[Byte] = {
      val crc = new java.util.zip.CRC32; crc.update(content)
      content ++ ByteBuffer.allocate(4).putInt(crc.getValue.toInt).array()
    }
    // Independent empty v2 body: ASCII magic, docs=0, terms=0, BE CRC32.
    val empty = checksummed("PRWSIDX2".getBytes("US-ASCII") ++ new Array[Byte](8))
    val f = dir.resolve("snapshot.segment").toFile
    Files.write(f.toPath, empty); assert(readSeg(f)._1.isEmpty)
    empty.indices.foreach { n =>
      rejected(f, empty.take(n))
      val corrupt = empty.clone(); corrupt(n) = (corrupt(n) ^ 1).toByte; rejected(f, corrupt)
    }
    rejected(f, empty ++ Array[Byte](0))
    rejected(f, checksummed("PRWSIDX3".getBytes("US-ASCII") ++ new Array[Byte](8)))
    rejected(f, checksummed("PRWSIDX2".getBytes("US-ASCII") ++ Array.fill[Byte](4)(-1) ++ new Array[Byte](4)))
    val invalidUtf8 = new ByteArrayOutputStream
    val data = new DataOutputStream(invalidUtf8)
    data.write("PRWSIDX2".getBytes("US-ASCII")); data.writeInt(1); data.writeLong(1)
    data.writeInt(1); data.writeByte(255); data.writeInt(0); data.writeInt(0); data.writeInt(0); data.close()
    rejected(f, checksummed(invalidUtf8.toByteArray))
    writeSeg(f, mutable.LongMap.empty[Doc], mutable.Map.empty[String, mutable.LongMap[Int]])
    assert(Files.readAllBytes(f.toPath).sameElements(empty), "writer differs from independent empty vector")
  }
  test("file length and legacy string validity checked before acceptance") { dir =>
    val f = dir.resolve("large.segment").toFile
    val random = new java.io.RandomAccessFile(f, "rw")
    try random.setLength(SnapshotStore.MaxFileBytes + 1) finally random.close()
    assert(scala.util.Try(readSeg(f)).isFailure)
    rejected(f, legacy(Seq(1L -> doc.copy(title = "\ud800")), Seq.empty))
  }
  test("corrupt latest fails recovery without falling back and releases owner") { dir =>
    val index = Files.createDirectory(dir.resolve("index"))
    Files.write(index.resolve("seg-00000.segment"), legacy(Seq(1L -> doc), Seq.empty))
    val latest = index.resolve("seg-00001.segment")
    Files.write(latest, Array[Byte](0))
    val s = new Session(dir)
    try {
      assert(scala.util.Try(s.start()).isFailure, "corruption silently fell back to older acknowledged state")
      Files.write(latest, legacy(Seq(1L -> doc.copy(title = "repaired")), Seq("alpha" -> Seq(1L -> 1))))
      s.start(); assert(s.query.contains("repaired"))
    } finally s.stop()
  }
  test("failed Clear cannot acknowledge deletion or accept later writes") { dir =>
    val s = new Session(dir)
    val index = dir.resolve("index"); val backup = dir.resolve("saved")
    try {
      s.start(); s.putDoc("retained"); s.putTerm(); s.ok("Flush")
      Files.move(index, backup); Files.write(index, Array[Byte](0))
      try {
        assert(s.request("Clear").isInstanceOf[BridgeReplyErr])
        assert(s.request("IndexTerms", i32(0)).isInstanceOf[BridgeReplyErr])
      } finally { s.stop(); Files.delete(index); Files.move(backup, index) }
      s.start(); assert(s.query.contains("retained"), s.query)
    } finally s.stop()
  }
  test("real TCP deployment rejects corrupt snapshot then redeploys repaired storage") { dir =>
    import parrot.protocol.jvm.AdminV2Codec.AdminReplyV2._
    import parrot.protocol.jvm.AdminV2Codec.AdminCommandV2._
    val index = Files.createDirectory(dir.resolve("index"))
    val latest = index.resolve("seg-00000.segment")
    Files.write(latest, Array[Byte](0))
    val admin = new AdminSession(dir)
    try {
      admin.start()
      assert(admin.deploy("storage-start").isInstanceOf[Failed], "corrupt snapshot returned Deployed")
      assert(admin.command(Status(2, "/user/storage-start")).isInstanceOf[Failed])
      assert(admin.routes.get("storage-start").isEmpty)
      Files.write(latest, legacy(Seq(1L -> doc), Seq.empty))
      assert(admin.deploy("storage-start").isInstanceOf[Deployed])
      assert(admin.health("storage-start").contains("\"docs\":1"))
      assert(admin.command(Stop(3, "/user/storage-start")).isInstanceOf[Stopped])
    } finally admin.close()
  }
  test("real TCP deployment rejects occupied storage and succeeds after owner retirement") { dir =>
    import parrot.protocol.jvm.AdminV2Codec.AdminReplyV2._
    import parrot.protocol.jvm.AdminV2Codec.AdminCommandV2._
    val admin = new AdminSession(dir)
    try {
      admin.start()
      assert(admin.deploy("storage-owner").isInstanceOf[Deployed])
      assert(admin.deploy("storage-contender").isInstanceOf[Failed], "second writer returned Deployed")
      assert(admin.health("storage-owner").contains("\"docs\":0"))
      assert(admin.routes.get("storage-contender").isEmpty)
      assert(admin.command(Stop(3, "/user/storage-owner")).isInstanceOf[Stopped])
      assert(admin.deploy("storage-contender").isInstanceOf[Deployed])
      assert(admin.health("storage-contender").contains("\"docs\":0"))
    } finally admin.close()
  }
  test("generation aliases and unrecognized segments fail closed") { dir =>
    val index = Files.createDirectory(dir.resolve("index"))
    Files.write(index.resolve("seg-1.segment"), legacy(Seq.empty, Seq.empty))
    val alias = index.resolve("seg-00001.segment")
    Files.write(alias, legacy(Seq.empty, Seq.empty))
    val s = new Session(dir)
    try {
      assert(scala.util.Try(s.start()).isFailure)
      Files.delete(alias); Files.write(index.resolve("unknown.segment"), legacy(Seq.empty, Seq.empty))
      assert(scala.util.Try(s.start()).isFailure)
    } finally s.stop()
  }
  for (mode <- Vector("write", "clear", "hold")) test(s"process SIGKILL after $mode releases lock and recovers committed state") { dir =>
    val javaExecutable = new File(System.getProperty("java.home"), "bin/java").toString
    val process = new ProcessBuilder(javaExecutable, "-cp", System.getProperty("java.class.path"),
      "websearch.search.StorageCrashProcess", dir.toString, mode).redirectError(ProcessBuilder.Redirect.INHERIT).start()
    val reader = new java.io.BufferedReader(new java.io.InputStreamReader(process.getInputStream, "UTF-8"))
    val executor = java.util.concurrent.Executors.newSingleThreadExecutor()
    val s = new Session(dir)
    try {
      val ready = executor.submit(new java.util.concurrent.Callable[Boolean] {
        override def call(): Boolean = {
          var line = reader.readLine(); var lines = 0
          while (line != null && line != "STORAGE_READY" && lines < 100) { line = reader.readLine(); lines += 1 }
          line == "STORAGE_READY"
        }
      })
      assert(ready.get(30, java.util.concurrent.TimeUnit.SECONDS), "child exited before commit acknowledgement")
      assert(scala.util.Try(s.start()).isFailure, "second process bypassed storage ownership")
      process.destroyForcibly(); assert(process.waitFor(10, java.util.concurrent.TimeUnit.SECONDS))
      s.start()
      if (mode == "write") { assert(s.query.contains("durable"), s.query); assert(s.health.contains("\"docs\":1")) }
      else { assert(s.query == "[]"); assert(s.health.contains("\"docs\":0")) }
    } finally {
      s.stop(); process.destroyForcibly(); process.waitFor(10, java.util.concurrent.TimeUnit.SECONDS)
      reader.close(); executor.shutdownNow()
    }
  }
  println(s"STORAGE_CONTRACTS passed=$passed failed=$failed")
  assert(failed == 0)

  /** Actual Netty connection exercises admin readiness, storage ownership and business routing. */
  class AdminSession(dir: Path) {
    import akka.actor.typed.scaladsl.Behaviors
    import parrot.protocol.jvm.{AdminPort, AdminV2Codec, AdminV2Executor, BridgeActor, WireFrame, ParrotTransportExtension, ParrotServerHandler}
    import AdminV2Codec._
    val system = ActorSystem[Any](Behaviors.empty, "storage-admin-contract")
    val admin = system.systemActorOf(AdminPort(system), "admin")
    val routes = AdminV2Executor.ComponentRoutes(system)
    val bridge = system.systemActorOf(BridgeActor(routes.get), "bridge")
    val transport = new ParrotTransportExtension(system, bridge, "storage-admin-contract", Some(admin))
    transport.listen(0)
    val socket = new java.net.Socket("127.0.0.1", transport.port)
    socket.setSoTimeout(5000)
    val input = new java.io.DataInputStream(socket.getInputStream)
    val output = socket.getOutputStream
    var cid = 0L
    def exchange(ft: Byte, path: String, key: String, bytes: Array[Byte], commandId: Option[Long] = None): WireFrame.Frame = {
      cid += 1
      val requestId = commandId.getOrElse(cid)
      output.write(WireFrame.Frame(1, ft, 0, requestId, 0, 8, path, key, bytes).encode()); output.flush()
      val header = new Array[Byte](4); input.readFully(header)
      val size = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN).getInt
      assert(size >= 28 && size <= 16 * 1024 * 1024)
      val body = new Array[Byte](size); input.readFully(body)
      val frame = WireFrame.decode(header ++ body).get._1
      assert(ft == WireFrame.FrameType.HANDSHAKE || frame.correlationId == requestId, s"wrong correlation: ${frame.correlationId}, expected $requestId")
      frame
    }
    def start(): Unit = assert(exchange(WireFrame.FrameType.HANDSHAKE, "", "", WireFrame.handshakeBody("storage-test"))
      .frameType == WireFrame.FrameType.HANDSHAKE_ACK)
    def command(value: AdminCommandV2): AdminReplyV2 = {
      val response = exchange(WireFrame.FrameType.SYSTEM_EVENT, "/_admin", "", encodeCommand(value), Some(value.reqId))
      assert(response.frameType == WireFrame.FrameType.SYSTEM_EVENT)
      val reply = decodeReply(response.payload)
      assert(reply.reqId == value.reqId)
      reply
    }
    def deploy(name: String): AdminReplyV2 = command(AdminCommandV2.Deploy(1, ComponentDeploy(
      name, "test", ArtifactRef.Jvm(classOf[SearchComponent].getName, None), InstancePolicy.Singleton,
      Some(s"data_dir = \"$dir\"".getBytes("UTF-8")))))
    def health(name: String): String = {
      val reply = exchange(WireFrame.FrameType.ASK, "/user/" + name, "bin:ws/Healthz",
        WireFrame.withReplyToPrefix("/test/health", Array.emptyByteArray))
      assert(reply.frameType == WireFrame.FrameType.REPLY && reply.path == "/test/health")
      new String(reply.payload, "UTF-8")
    }
    def close(): Unit = {
      socket.close(); transport.shutdown().get(10, java.util.concurrent.TimeUnit.SECONDS)
      system.terminate(); Await.result(system.whenTerminated, 10.seconds)
    }
  }

  class Session(dir: Path) {
    var system: ActorSystem[Any] = null
    def start(): Unit = {
      require(system == null)
      val behavior = new SearchComponent().behavior(ComponentContext("/user/search", "test", Some(s"data_dir = \"$dir\"".getBytes("UTF-8"))))
      system = ActorSystem(behavior, "storage-contract")
    }
    def stop(): Unit = if (system != null) {
      system.terminate(); Await.result(system.whenTerminated, 10.seconds); system = null
    }
    def request(key: String, p: Array[Byte] = Array.emptyByteArray): Any = {
      implicit val timeout: akka.util.Timeout = 1.second
      implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
      Await.result(system.ask[Any](ref => BridgeAsk("bin:ws/" + key, p, ref)), 2.seconds)
    }
    def ok(key: String, p: Array[Byte] = Array.emptyByteArray): Array[Byte] = request(key, p) match {
      case BridgeReplyOk(_, b) => b
      case other => throw new AssertionError(s"unexpected $key reply: $other")
    }
    def putDoc(title: String): Unit = { ok("DocMeta", i64(1) ++ str("https://a.test") ++ str(title) ++ str("alpha")); () }
    def putTerm(): Unit = { ok("IndexTerms", i32(1) ++ str("alpha") ++ i64(1) ++ i32(1)); () }
    def health: String = new String(ok("Healthz"), "UTF-8")
    def query: String = new String(ok("Search", i32(10) ++ str("alpha")), "UTF-8")
  }
}
