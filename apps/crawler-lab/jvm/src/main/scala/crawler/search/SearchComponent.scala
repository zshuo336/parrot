/** crawler-lab 搜索组件（jvm 方言——app 内 source of truth）。
  *
  * R1（应用体系架构纠正）：本类是 apps/crawler-lab 的业务代码，经 parrot
  * 标准包分发：crawler.app.toml 声明 artifact = { Jvm = { main_class =
  * "crawler.search.SearchComponent", coords = "file://...jar" } }，jvm 网关
  * deploy 时经 child-first loader 载入本类（ComponentSpi）并 spawn。
  *
  * 协议（与 Rust hub 逐字节对齐——run_regression golden 锚定）：
  *   bin:crawl/IndexTerms → IndexAck [n u32]
  *   bin:crawl/Search     → SearchResult json
  *   bin:crawl/Healthz    → HealthzR json
  */
package crawler.search

import akka.actor.typed.Behavior
import akka.actor.typed.scaladsl.Behaviors
import parrot.protocol.jvm.{ComponentContext, ComponentSpi}
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk}

class SearchComponent extends ComponentSpi {
  override def behavior(ctx: ComponentContext): Behavior[Any] = SearchBehavior.searchActor(ctx, 5)
}

object SearchBehavior {
  private def u32(p: Array[Byte], off: Int): Int =
    (p(off).toInt & 0xFF) | ((p(off + 1).toInt & 0xFF) << 8) |
      ((p(off + 2).toInt & 0xFF) << 16) | ((p(off + 3).toInt & 0xFF) << 24)
  private def u64(p: Array[Byte], off: Int): Long = {
    var v = 0L
    var b = 0
    while (b < 8) { v |= (p(off + b).toLong & 0xFF) << (8 * b); b += 1 }
    v
  }

  def searchActor(ctx: ComponentContext, k: Int): Behavior[Any] = {
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
}
