package parrot.apps.contracts

import akka.actor.typed.{ActorSystem, ActorRef}
import akka.actor.typed.scaladsl.{Behaviors, AskPattern}
import AskPattern._
import parrot.protocol.jvm.{ComponentContext, ComponentSpi, BridgeActor, WireFrame, ParrotTransportExtension, ParrotServerHandler}
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk}
import java.nio.{ByteBuffer, ByteOrder}
import java.nio.file.Files
import scala.concurrent.Await
import scala.concurrent.duration._
import scala.jdk.CollectionConverters._

/** Independent bytes exercise real application actors; every rejected request is
  * followed by a health barrier on the SAME actor, with unchanged state. */
object JvmRequestContracts extends App {
  def i32(n: Int): Array[Byte] = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(n).array()
  def i64(n: Long): Array[Byte] = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(n).array()
  def str(s: String): Array[Byte] = { val b = s.getBytes("UTF-8"); i32(b.length) ++ b }
  def terms(xs: (String, Long, Int)*): Array[Byte] = i32(xs.size) ++ xs.flatMap {
    case (t, d, f) => str(t) ++ i64(d) ++ i32(f)
  }.toArray
  var passed = 0
  var failed = 0
  for (app <- Vector("crawler", "websearch")) {
    def test(name: String)(f: Session => Unit): Unit = {
      val s = new Session(app)
      try { f(s); passed += 1; println(s"PASS $app $name") }
      catch { case scala.util.control.NonFatal(e) => failed += 1; println(s"FAIL $app $name: $e") }
      finally s.close()
    }
    test("all IndexTerms truncations preserve state and actor") { s =>
      val p = terms(("alpha", 1L, 2), ("中文", 2L, 3))
      (0 until p.length).foreach(n => s.reject("IndexTerms", p.take(n)))
      assert(s.ok("IndexTerms", p).sameElements(i32(2)))
      assert(s.health.contains("\"postings\":2"))
    }
    test("count tail UTF8 and budget rejection") { s =>
      Vector(i32(-1), i32(65537), i32(0) ++ Array[Byte](0),
        i32(1) ++ i32(1) ++ Array[Byte](-1) ++ i64(1) ++ i32(2),
        i32(1) ++ i32(-1)).foreach(s.reject("IndexTerms", _))
      val record = str("") ++ i64(1) ++ i32(1)
      s.reject("IndexTerms", i32(65537) ++ Array.fill(65537)(record).flatten)
      s.reject("IndexTerms", new Array[Byte](16 * 1024 * 1024 + 1))
      assert(s.ok("IndexTerms", i32(0)).sameElements(i32(0)))
    }
    test("empty and fixed request shapes") { s =>
      s.reject("Healthz", Array[Byte](0))
      if (app == "websearch") {
        Vector("Clear", "Flush").foreach(s.reject(_, Array[Byte](0)))
        Vector("ListDocs", "ListTerms").foreach { key =>
          val p = i32(0) ++ i32(10)
          (0 until p.length).foreach(n => s.reject(key, p.take(n)))
          s.reject(key, p ++ Array[Byte](0)); s.ok(key, p)
        }
      }
    }
    test("Search complete consumption UTF8 and no failed-query count") { s =>
      val p = s.query("alpha")
      (0 until p.length).foreach(n => s.reject("Search", p.take(n)))
      s.reject("Search", p ++ Array[Byte](0))
      s.reject("Search", i32(1) ++ i32(1) ++ Array[Byte](-1))
      assert(new String(s.ok("Search", s.query("中文😀")), "UTF-8") == "[]")
      assert(s.health.contains("\"queries\":1"))
    }
    test("unknown request has explicit error and recovery") { s =>
      s.reject("Unknown", Array.emptyByteArray, 6)
      assert(s.ok("IndexTerms", terms(("alpha", 1L, 1))).sameElements(i32(1)))
    }
    test("unsigned term frequency and repeated assignments") { s =>
      if (app == "websearch") s.ok("DocMeta", i64(1) ++ str("https://a.test") ++ str("alpha") ++ str("alpha"))
      val p = terms(("alpha", 1L, -1))
      s.ok("IndexTerms", p)
      val before = new String(s.ok("Search", s.query("alpha")), "UTF-8")
      if (app == "crawler") assert(before == "[{\"doc\":1,\"score\":4294967295}]", before)
      else assert(before.contains("\"score\":0.63"), before)
      s.ok("IndexTerms", p)
      assert(new String(s.ok("Search", s.query("alpha")), "UTF-8") == before)
      assert(s.health.contains("\"postings\":1"))
      if (app == "websearch") {
        val hugeK = i32(-1) ++ str("alpha")
        assert(new String(s.ok("Search", hugeK), "UTF-8") == before)
        assert(new String(s.ok("ListTerms", i32(0) ++ i32(-1)), "UTF-8").contains("alpha"))
      }
    }
    test("real TCP REPLY_ERR correlation and same-connection recovery") { s =>
      import WireFrame.FrameType
      val bridge = s.system.systemActorOf(BridgeActor(path => if (path == "search") Some(s.target) else None), "bridge")
      val transport = new ParrotTransportExtension(s.system, bridge, "request-contract")
      var socket: java.net.Socket = null
      try {
        transport.listen(0)
        socket = new java.net.Socket("127.0.0.1", transport.port)
        socket.setSoTimeout(3000)
        val input = new java.io.DataInputStream(socket.getInputStream)
        val output = socket.getOutputStream
        def exchange(ft: Byte, cid: Long, key: String, payload: Array[Byte]): WireFrame.Frame = {
          output.write(WireFrame.Frame(1, ft, 0, cid, 0, 8, "/user/search", key, payload).encode())
          output.flush()
          val header = new Array[Byte](4); input.readFully(header)
          val length = ByteBuffer.wrap(header).order(ByteOrder.LITTLE_ENDIAN).getInt
          assert(length >= 28 && length <= 16 * 1024 * 1024)
          val body = new Array[Byte](length); input.readFully(body)
          WireFrame.decode(header ++ body).get._1
        }
        val handshake = exchange(FrameType.HANDSHAKE, 0, "", WireFrame.handshakeBody("test-client"))
        assert(handshake.frameType == FrameType.HANDSHAKE_ACK)
        def ask(cid: Long, key: String, p: Array[Byte]): WireFrame.Frame =
          exchange(FrameType.ASK, cid, s.prefix + key, WireFrame.withReplyToPrefix("/test/replies", p))
        for ((key, payload, code, cid) <- Vector(
          ("IndexTerms", terms(("alpha", 1L, 1), ("beta", 2L, 1)).dropRight(1), 5, 41L),
          ("Search", i32(1), 5, 42L), ("Unknown", Array.emptyByteArray, 6, 43L))) {
          val reply = ask(cid, key, payload)
          assert(reply.frameType == FrameType.REPLY_ERR && reply.correlationId == cid && reply.path == "/test/replies")
          assert(WireFrame.decodeErrPayload(reply.payload)._1 == code)
        }
        val state = ask(44, "Healthz", Array.emptyByteArray)
        assert(state.frameType == FrameType.REPLY && new String(state.payload, "UTF-8").contains("\"postings\":0"))
        val good = ask(45, "IndexTerms", terms(("alpha", 1L, 1)))
        assert(good.frameType == FrameType.REPLY && good.correlationId == 45 && good.payload.sameElements(i32(1)))
        val finalState = ask(46, "Healthz", Array.emptyByteArray)
        assert(new String(finalState.payload, "UTF-8").contains("\"postings\":1"))
      } finally {
        if (socket != null) socket.close()
        transport.shutdown().get(10, java.util.concurrent.TimeUnit.SECONDS)
      }
    }
    if (app == "websearch") test("DocMeta full validation and Unicode snippet") { s =>
      val p = i64(-1) ++ str("https://中文.test") ++ str("中文😀") ++ str("😀" * 300)
      (0 until p.length).foreach(n => s.reject("DocMeta", p.take(n)))
      s.reject("DocMeta", p ++ Array[Byte](0))
      s.reject("DocMeta", i64(1) ++ str("u") ++ str("t") ++ i32(1) ++ Array[Byte](-1))
      s.ok("DocMeta", p)
      s.ok("IndexTerms", terms(("alpha", -1L, 1)))
      val json = new String(s.ok("Search", s.query("alpha")), "UTF-8")
      assert(json.contains("\"snippet\":\"" + ("😀" * 280) + "\""), json)
      assert(!json.contains("�") && !json.contains("?"), json)
    }
  }
  println(s"JVM_REQUEST_CONTRACTS passed=$passed failed=$failed")
  assert(failed == 0)

  class Session(app: String) {
    val dir = Files.createTempDirectory("jvm-request-contract-")
    val system = ActorSystem[Any](Behaviors.empty, "request-contract")
    val component: ComponentSpi = if (app == "websearch") new websearch.search.SearchComponent else new crawler.search.SearchComponent
    val target = system.systemActorOf(component.behavior(ComponentContext("/user/search", "test",
      Some(s"data_dir = \"$dir\"".getBytes("UTF-8")))), "search")
    val prefix = if (app == "websearch") "bin:ws/" else "bin:crawl/"
    implicit val timeout: akka.util.Timeout = 1.second
    implicit val scheduler: akka.actor.typed.Scheduler = system.scheduler
    def request(key: String, p: Array[Byte]): Any = Await.result(target.ask[Any](r => BridgeAsk(prefix + key, p, r)), 2.seconds)
    def ok(key: String, p: Array[Byte]): Array[Byte] = request(key, p) match {
      case BridgeReplyOk(_, b) => b
      case other => throw new AssertionError(s"expected successful $key, got $other")
    }
    def health: String = new String(ok("Healthz", Array.emptyByteArray), "UTF-8")
    def reject(key: String, p: Array[Byte], code: Int = 5): Unit = {
      val before = health
      val reply = request(key, p)
      // Product check also compiles against the pre-fix gateway to reproduce failures.
      assert(reply.isInstanceOf[Product], s"missing structured error: $reply")
      val error = reply.asInstanceOf[Product]
      assert(error.productPrefix == "BridgeReplyErr" && error.productElement(0) == code, s"bad error: $reply")
      assert(health == before, s"$key changed state on rejected payload")
    }
    def query(q: String): Array[Byte] = i32(if (app == "websearch") 10 else 1) ++ str(q)
    def close(): Unit = {
      system.terminate(); Await.result(system.whenTerminated, 10.seconds)
      val paths = Files.walk(dir)
      try paths.iterator().asScala.toVector.reverse.foreach(Files.delete) finally paths.close()
    }
  }
}
