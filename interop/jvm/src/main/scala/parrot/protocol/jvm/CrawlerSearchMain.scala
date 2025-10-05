package parrot.protocol.jvm

import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorSystem, Behavior}
import BridgeActor._
import scala.concurrent.duration._

/** crawler-lab JVM 入口：搜索 API actor（倒排接收 + top-k 打分 + JSON 响应）。
  *
  * 场景角色：面向用户 Web 访问的检索层（akka 承接——高并发短查询强项）。
  *
  * actor：search（三协议分支：IndexTerms 入库 / Search 查询 / Healthz 探针）
  *        echo/cpu（与 ParrotGatewayMain 同语义——回归兼容）
  *
  * 载荷布局（与 Rust 侧 parrot-crawler-lab.rs 逐字节对齐）：
  *   IndexTerms: [n u32][{len u32|term utf8|doc u64|tf u32}...]
  *   Search:     [k u32][{len u32|term utf8}...]
  *   Healthz:    空 → JSON {"terms","postings","queries"}
  */
object CrawlerSearchMain {

  // LE 读助手（payload 小端整数）
  private def u32(p: Array[Byte], off: Int): Int =
    (p(off).toInt & 0xFF) | ((p(off + 1).toInt & 0xFF) << 8) |
      ((p(off + 2).toInt & 0xFF) << 16) | ((p(off + 3).toInt & 0xFF) << 24)
  private def u64(p: Array[Byte], off: Int): Long = {
    var v = 0L
    var b = 0
    while (b < 8) { v |= (p(off + b).toLong & 0xFF) << (8 * b); b += 1 }
    v
  }

  case class SearchState(
      inverted: scala.collection.mutable.Map[String, Array[(Long, Int)]],
      queries: Long
  )

  def searchActor(k: Int): Behavior[Any] = {
    var inverted = scala.collection.mutable.Map[String, Array[(Long, Int)]]()
    var queries  = 0L

    def handleIndexTerms(payload: Array[Byte], replyTo: akka.actor.typed.ActorRef[Any]): Unit = {
      var off = 0
      val n = u32(payload, off); off += 4
      var i = 0
      while (i < n) {
        val tlen = u32(payload, off); off += 4
        val term = new String(payload, off, tlen, "UTF-8"); off += tlen
        val doc = u64(payload, off); off += 8
        val tf = u32(payload, off); off += 4
        inverted.update(term, inverted.getOrElse(term, Array.empty) :+ ((doc, tf)))
        i += 1
      }
      val ack = new Array[Byte](4)
      ack(0) = (n & 0xFF).toByte; ack(1) = ((n >> 8) & 0xFF).toByte
      ack(2) = ((n >> 16) & 0xFF).toByte; ack(3) = ((n >> 24) & 0xFF).toByte
      replyTo ! BridgeReplyOk("bin:crawl/IndexAck", ack)
    }

    def handleSearch(payload: Array[Byte], replyTo: akka.actor.typed.ActorRef[Any]): Unit = {
      queries += 1
      var off = 0
      val nq = u32(payload, off); off += 4
      val terms = (0 until nq).map { _ =>
        val tlen = u32(payload, off); off += 4
        val t = new String(payload, off, tlen, "UTF-8"); off += tlen
        t
      }
      val scores = scala.collection.mutable.Map[Long, Int]()
      terms.foreach { t =>
        inverted.get(t).foreach { ps =>
          ps.foreach { case (d, tf) => scores.update(d, scores.getOrElse(d, 0) + tf) }
        }
      }
      val top = scores.toSeq.sortBy(-_._2).take(k)
      val json = top.map { case (d, s) => s"""{"doc":$d,"score":$s}""" }.mkString("[", ",", "]")
      replyTo ! BridgeReplyOk("bin:crawl/SearchResult", json.getBytes("UTF-8"))
    }

    def handleHealthz(replyTo: akka.actor.typed.ActorRef[Any]): Unit = {
      val postings = inverted.values.map(_.length).sum
      val json = s"""{"terms":${inverted.size},"postings":$postings,"queries":$queries}"""
      replyTo ! BridgeReplyOk("bin:crawl/HealthzR", json.getBytes("UTF-8"))
    }

    Behaviors.receiveMessage[Any] {
      case BridgeAsk(key, payload, replyTo) =>
        key match {
          case "bin:crawl/IndexTerms" => handleIndexTerms(payload, replyTo)
          case "bin:crawl/Search"     => handleSearch(payload, replyTo)
          case "bin:crawl/Healthz"    => handleHealthz(replyTo)
          case _                      =>
        }
        Behaviors.same
      case _ => Behaviors.same
    }
  }

  def main(args: Array[String]): Unit = {
    // 双模式组网：
    //   [port]              —— 被动模式（listen 等 parrot 拨入）
    //   [port, parrot=h:p]  —— 注册模式（主动拨号 parrot 应用并驻留）
    //   [port, idle]        —— 被动 + idle 秒超时
    val port = if (args.length > 0) args(0).toInt else 0
    val parrotReg = args.find(_.startsWith("parrot=")).map(_.stripPrefix("parrot="))
    val idle = args
      .drop(1)
      .find(_.forall(_.isDigit))
      .map(_.toInt)
      .getOrElse(3600)

    val guardian = Behaviors.setup[BridgeMsg] { ctx =>
      val search = ctx.spawn(searchActor(5), "search")
      val targets: Map[String, akka.actor.typed.ActorRef[Any]] =
        Map("search" -> search)
      val bridge = ctx.spawn(BridgeActor(path => targets.get(path)), "bridge")
      val ext = new ParrotTransportExtension(ctx.system, bridge, "jvm-search-1")
      ParrotServerHandler.initSystem(ctx.system)
      parrotReg match {
        case Some(target) =>
          // 注册模式：注册到 parrot 后驻留（registerTo 内部 sync 等待连接关闭）
          val Array(host, p) = target.split(":")
          System.out.println(s"PARROT_JVM_REGISTERING=$target")
          System.out.flush()
          ctx.executionContext.execute(() =>
            ext.registerTo(host, p.toInt)
          )
        case None =>
          ext.listen(port)
          System.out.println(s"PARROT_JVM_PORT=${ext.port}")
          System.out.flush()
      }
      Behaviors.empty
    }
    val sys = ActorSystem(guardian, "parrot-crawler")
    sys.scheduler.scheduleOnce(idle.seconds, () => sys.terminate())(sys.executionContext)
  }
}
