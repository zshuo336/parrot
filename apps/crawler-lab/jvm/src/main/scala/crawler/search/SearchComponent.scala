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
import parrot.protocol.jvm.{ComponentContext, ComponentSpi, PayloadReader, WireFrame}
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk, BridgeReplyErr}

class SearchComponent extends ComponentSpi {
  override def behavior(ctx: ComponentContext): Behavior[Any] = SearchBehavior.searchActor(ctx, 5)
}

object SearchBehavior {
  def searchActor(ctx: ComponentContext, k: Int): Behavior[Any] = {
    val inverted = scala.collection.mutable.Map.empty[String, scala.collection.mutable.LongMap[Long]]
    var queries  = 0L

    def handleIndexTerms(payload: Array[Byte], replyTo: akka.actor.typed.ActorRef[Any]): Unit = {
      val reader = new PayloadReader(payload)
      val n = reader.count(16)
      val entries = Vector.fill(n)((reader.utf8(), reader.u64(), reader.u32()))
      reader.finish()
      entries.foreach { case (term, doc, tf) =>
        inverted.getOrElseUpdate(term, scala.collection.mutable.LongMap.empty[Long]).update(doc, tf)
      }
      val ack = new Array[Byte](4)
      ack(0) = (n & 0xFF).toByte; ack(1) = ((n >> 8) & 0xFF).toByte
      ack(2) = ((n >> 16) & 0xFF).toByte; ack(3) = ((n >> 24) & 0xFF).toByte
      replyTo ! BridgeReplyOk("bin:crawl/IndexAck", ack)
    }

    def handleSearch(payload: Array[Byte], replyTo: akka.actor.typed.ActorRef[Any]): Unit = {
      val reader = new PayloadReader(payload)
      val nq = reader.count(4)
      val terms = Vector.fill(nq)(reader.utf8())
      reader.finish()
      queries += 1
      val scores = scala.collection.mutable.LongMap.empty[Long]
      terms.foreach { t =>
        inverted.get(t).foreach { ps =>
          ps.foreach { case (d, tf) => scores.update(d, scores.getOrElse(d, 0L) + tf) }
        }
      }
      val top = scores.toSeq.sortBy { case (doc, score) => (-score, doc) }.take(k)
      val json = top.map { case (d, s) => s"""{"doc":$d,"score":$s}""" }.mkString("[", ",", "]")
      replyTo ! BridgeReplyOk("bin:crawl/SearchResult", json.getBytes("UTF-8"))
    }

    def handleHealthz(replyTo: akka.actor.typed.ActorRef[Any]): Unit = {
      val postings = inverted.valuesIterator.map(_.size.toLong).sum
      val json = s"""{"terms":${inverted.size},"postings":$postings,"queries":$queries}"""
      replyTo ! BridgeReplyOk("bin:crawl/HealthzR", json.getBytes("UTF-8"))
    }

    Behaviors.receiveMessage[Any] {
      case BridgeAsk(key, payload, replyTo) =>
        try {
          key match {
            case "bin:crawl/IndexTerms" => handleIndexTerms(payload, replyTo)
            case "bin:crawl/Search"     => handleSearch(payload, replyTo)
            case "bin:crawl/Healthz"    => new PayloadReader(payload).finish(); handleHealthz(replyTo)
            case _ => replyTo ! BridgeReplyErr(WireFrame.ErrCode.UnknownTypeKey, "unknown search request")
          }
        } catch {
          case e: PayloadReader.InvalidPayload => replyTo ! BridgeReplyErr(WireFrame.ErrCode.CodecError, e.getMessage)
        }
        Behaviors.same
      case _ => Behaviors.same
    }
  }
}
