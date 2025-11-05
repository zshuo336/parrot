/** websearch 检索服务（akka 方言——用户查询 Web 入口）。
  *
  * 五运行时分工（用户裁定）：本组件 = 检索/用户查询侧——
  *   - jieba-analysis 中文分词（查询切词——与 ray jieba / rust jieba-rs 同族）
  *   - 倒排索引 + BM25 打分（k1=1.2 b=0.75）
  *   - 完整快照（data/index/seg-*.segment——Flush确认后恢复最新快照）
  *   - doc 元数据表（docId → url/title）+ 正文摘要缓存
  *
  * 协议（bin:ws 前缀键空间）：
  *   bin:ws/IndexDocuments [n u32][{docid|url|title|text|nterms u32|{term|tf u32}...}...] → DocumentsAck [n u32]
  *     Complete in-memory replacement; Flush is still required for durability.
  *   bin:ws/IndexTerms [n u32][{len u32|term|docid u64|tf u32}...] → Ack [n u32]
  *   bin:ws/DocMeta    [docid u64|len u32|url|len u32|title|len u32|text utf8] → Ack [1 u32]
  *   bin:ws/Search     [k u32|len u32|query utf8] → json [{doc,score,url,title,snippet}]
  *   bin:ws/Flush      [] → [segs u32]（段落盘命令——Rust 爬完调用）
  *   bin:ws/Healthz    [] → json {"terms","postings","queries","docs"}
  */
package websearch.search

import akka.actor.typed.{Behavior, PostStop}
import akka.actor.typed.scaladsl.Behaviors
import com.huaban.analysis.jieba.JiebaSegmenter
import parrot.protocol.jvm.{ComponentContext, ComponentSpi, PayloadReader, WireFrame}
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk, BridgeReplyErr}

import java.io.{File, IOException}
import scala.collection.mutable

class SearchComponent extends ComponentSpi {
  override def behavior(ctx: ComponentContext): Behavior[Any] =
    SearchBehavior.searchActor(ctx)
}

object SearchBehavior {
  private val seg = new JiebaSegmenter

  /** URL → host（与 Rust host_of 同规：小写）。 */
  private def hostOf(url: String): String = {
    val rest = url.split("://", 2) match { case Array(_, r) => r; case _ => url }
    rest.split('/').headOption.getOrElse("").toLowerCase
  }

  private val stop = Set(
    "的","了","和","是","在","也","有","就","不","人","都","一","一个",
    "我们","你们","他们","这","那","这个","那个","什么","没有","还有",
    "因为","所以","但是","如果","可以","那些","这些","而且","以及",
    "或者","并且","不过","然而","因此","于是","然后","还是","只是",
    "已经","www","com","http","https","html","页面","网站","链接","查看",
    "更多","相关","参考文献","外部","the","a","an","of","to","in","and",
    "or","for","on","with","at","by","is","it","as","be"
  )

  private def put32(n: Int): Array[Byte] = Array(
    (n & 0xFF).toByte, ((n >> 8) & 0xFF).toByte,
    ((n >> 16) & 0xFF).toByte, ((n >> 24) & 0xFF).toByte)

  private def pageOffset(raw: Int, total: Int, limit: Int): Int = {
    val lastStart = if (total == 0) 0L else ((total.toLong - 1) / limit) * limit
    math.min(java.lang.Integer.toUnsignedLong(raw), lastStart).toInt
  }

  /** jieba 查询切词（与索引侧同停用词表）。 */
  def tokenize(text: String): Vector[String] = seg.synchronized {
    val words = scala.collection.mutable.ArrayBuffer.empty[String]
    seg.process(text, JiebaSegmenter.SegMode.SEARCH).forEach { w =>
      words += w.word.trim
    }
    // 拉丁词直接来自 jieba；另兜底切 CJK（jieba SEARCH 模式已含单字/词组）
    words
      .filter(w => w.length > 1 && !stop.contains(w) && !w.forall(_.isWhitespace))
      .distinct
      .toVector
  }

  case class Doc(url: String, title: String, snippet: String)

  def writeSeg(f: File, docs: mutable.LongMap[Doc],
               postings: mutable.Map[String, mutable.LongMap[Int]]): Unit =
    SnapshotStore.write(f, docs, postings)

  def readSeg(f: File): (mutable.LongMap[Doc], mutable.Map[String, mutable.LongMap[Int]]) =
    SnapshotStore.read(f)

  def searchActor(ctx: ComponentContext): Behavior[Any] = {
    // 数据目录：组件 config（TOML 片段）或环境变量 WS_DATA，缺省 ./data
    val dataDir = SearchConfig.directory(ctx.config, sys.env.get("WS_DATA"))
    val store = SnapshotStore.open(new File(dataDir, "index"))
    val (docs, postings) = store.recovered
    val docLen = mutable.LongMap.empty[Long]
    val documentTerms = mutable.LongMap.empty[mutable.Set[String]]
    var queries = 0L
    var storageFailed = false
    // Every generation is a full snapshot; only the latest is authoritative.
    postings.foreach { case (term, ds) =>
      ds.foreach { case (id, tf) =>
        docLen.update(id, docLen.getOrElse(id, 0L) + java.lang.Integer.toUnsignedLong(tf))
        documentTerms.getOrElseUpdate(id, mutable.Set.empty[String]) += term
      }
    }

    def document(url: String, title: String, text: String): Doc = {
      val normalized = text.replaceAll("\\s+", " ")
      val end = normalized.offsetByCodePoints(0, math.min(280, normalized.codePointCount(0, normalized.length)))
      Doc(url, title, normalized.substring(0, end))
    }
    def assign(term: String, docId: Long, tf: Int): Unit = {
      val perTerm = postings.getOrElseUpdate(term, mutable.LongMap.empty[Int])
      val previous = perTerm.getOrElse(docId, 0)
      perTerm.update(docId, tf)
      documentTerms.getOrElseUpdate(docId, mutable.Set.empty[String]) += term
      docLen.update(docId, docLen.getOrElse(docId, 0L) +
        java.lang.Integer.toUnsignedLong(tf) - java.lang.Integer.toUnsignedLong(previous))
    }
    def removeTerms(docId: Long): Unit = {
      documentTerms.remove(docId).foreach(_.foreach { term =>
        val perTerm = postings(term)
        perTerm.remove(docId)
        if (perTerm.isEmpty) postings.remove(term)
      })
      docLen.remove(docId)
    }

    def checkpointBytes(): Array[Byte] = {
      val (epoch, generation) = store.crawlState
      epoch.toArray ++ java.nio.ByteBuffer.allocate(8).order(java.nio.ByteOrder.LITTLE_ENDIAN).putLong(generation).array()
    }

    def flushSegs(): Int = {
      // Metadata-only and empty indexes are valid durable states too.
      store.commit(docs, postings)
      1
    }

    def bm25(terms: Vector[String], k: Int): Vector[(Long, Double)] = {
      val n = docs.size.toDouble
      val avg = if (n == 0) 0.0 else docLen.values.sum.toDouble / n
      val k1 = 1.2; val b = 0.75
      val scores = mutable.LongMap.empty[Double]
      terms.foreach { t =>
        postings.get(t).foreach { ds =>
          val df = ds.size.toDouble
          val idf = math.log((n - df + 0.5) / (df + 0.5) + 1.0)
          ds.foreach { case (docId, rawTf) =>
            val tf = java.lang.Integer.toUnsignedLong(rawTf)
            val dl = math.max(docLen.getOrElse(docId, tf.toLong).toDouble, 1.0)
            val denom = tf + k1 * (1 - b + b * dl / math.max(avg, 1.0))
            scores.update(docId, scores.getOrElse(docId, 0.0) + idf * tf * (k1 + 1) / denom)
          }
        }
      }
      scores.toVector.sortBy { case (id, score) => (-score, id) }.take(k)
    }

    def esc(s: String): String = {
      val out = new StringBuilder
      s.foreach {
        case '\\' => out.append("\\\\")
        case '"' => out.append("\\\"")
        case c if c < ' ' => out.append(f"\\u${c.toInt}%04x")
        case c => out.append(c)
      }
      out.toString
    }

    Behaviors.receiveMessage[Any] {
      case BridgeAsk(key, payload, replyTo) =>
        try {
          if (storageFailed) throw new IOException("index storage failed; redeploy to recover before further requests")
          val reader = new PayloadReader(payload)
          key match {
            case "bin:ws/IndexDocuments" =>
              val count = reader.count(24)
              val ids = mutable.Set.empty[Long]
              var totalTerms = 0
              // Decode and validate the entire request before mutating any
              // document, metadata, reverse lookup or posting. Actor requests
              // are serial, so readers see complete replacements only.
              val replacements = Vector.fill(count) {
                val id = reader.u64()
                if (!ids.add(id)) throw new PayloadReader.InvalidPayload("duplicate document id")
                val value = document(reader.utf8(), reader.utf8(), reader.utf8())
                val n = reader.count(8, 65536 - totalTerms)
                totalTerms += n
                val terms = mutable.Set.empty[String]
                val words = Vector.fill(n) {
                  val word = reader.utf8(); val tf = reader.u32().toInt
                  if (word.isEmpty || tf == 0 || !terms.add(word))
                    throw new PayloadReader.InvalidPayload("empty/duplicate term or zero frequency")
                  (word, tf)
                }
                (id, value, words)
              }
              reader.finish()
              replacements.foreach { case (id, value, words) =>
                removeTerms(id)
                docs.update(id, value)
                words.foreach { case (term, tf) => assign(term, id, tf) }
              }
              replyTo ! BridgeReplyOk("bin:ws/DocumentsAck", put32(count))

            case "bin:ws/IndexTerms" =>
              val n = reader.count(16)
              val entries = Vector.fill(n)((reader.utf8(), reader.u64(), reader.u32().toInt))
              reader.finish()
              entries.foreach { case (term, docId, tf) =>
                assign(term, docId, tf)
              }
              replyTo ! BridgeReplyOk("bin:ws/IndexAck", put32(n))

            case "bin:ws/DocMeta" =>
              val docId = reader.u64()
              val url = reader.utf8()
              val title = reader.utf8()
              val text = reader.utf8()
              reader.finish()
              docs.update(docId, document(url, title, text))
              replyTo ! BridgeReplyOk("bin:ws/DocAck", put32(1))

            case "bin:ws/Search" =>
              val k = math.min(reader.u32(), Int.MaxValue.toLong).toInt
              val q = reader.utf8()
              reader.finish()
              val terms = tokenize(q)
              queries += 1
              val hits = bm25(terms, k)
              val json = hits.map { case (docId, score) =>
                docs.get(docId) match {
                  case Some(d) =>
                    s"""{"doc":$docId,"score":${String.format(java.util.Locale.ROOT, "%.2f", Double.box(score))},"url":"${esc(d.url)}","title":"${esc(d.title)}","snippet":"${esc(d.snippet)}"}"""
                  case None => s"""{"doc":$docId,"score":${String.format(java.util.Locale.ROOT, "%.2f", Double.box(score))}}"""
                }
              }.mkString("[", ",", "]")
              replyTo ! BridgeReplyOk("bin:ws/SearchResult", json.getBytes("UTF-8"))

            case "bin:ws/PrepareIndex" =>
              reader.finish()
              val (ready, verified) = store.prepareHistory()
              val progress = Array[Byte](if (ready) 1 else 0) ++ java.nio.ByteBuffer.allocate(8)
                .order(java.nio.ByteOrder.LITTLE_ENDIAN).putLong(verified).array()
              replyTo ! BridgeReplyOk("bin:ws/PrepareIndexR", progress)

            case "bin:ws/CrawlStateV2" =>
              if (payload.nonEmpty && payload.length != 56)
                throw new PayloadReader.InvalidPayload("crawl history request must be empty or exactly 56 bytes")
              if (payload.nonEmpty) {
                val generation = java.nio.ByteBuffer.wrap(payload, 16, 8).order(java.nio.ByteOrder.LITTLE_ENDIAN).getLong
                if (!store.containsCheckpoint(payload.take(16).toVector, generation, payload.drop(24).toVector))
                  throw new PayloadReader.InvalidPayload("crawl journal/index generation mismatch or history mismatch")
              }
              store.ensureCrawlState(docs, postings)
              replyTo ! BridgeReplyOk("bin:ws/CrawlStateV2R", checkpointBytes() ++ store.crawlDigest.toArray)

            case "bin:ws/CheckpointV2" =>
              if (payload.length != 56) throw new PayloadReader.InvalidPayload("checkpoint history must be exactly 56 bytes")
              val generation = java.nio.ByteBuffer.wrap(payload, 16, 8).order(java.nio.ByteOrder.LITTLE_ENDIAN).getLong
              if (!store.containsCheckpoint(payload.take(16).toVector, generation, payload.drop(24).toVector))
                throw new PayloadReader.InvalidPayload("checkpoint history mismatch")
              store.commit(docs, postings)
              replyTo ! BridgeReplyOk("bin:ws/CheckpointV2Ack", checkpointBytes() ++ store.crawlDigest.toArray)

            case "bin:ws/CrawlState" =>
              reader.finish()
              // Upgrade a legacy snapshot before returning a durable identity.
              store.ensureCrawlState(docs, postings)
              replyTo ! BridgeReplyOk("bin:ws/CrawlStateR", checkpointBytes())

            case "bin:ws/Checkpoint" =>
              if (payload.length != 16) throw new PayloadReader.InvalidPayload("checkpoint epoch must be exactly 16 bytes")
              if (!store.matchesEpoch(payload)) throw new PayloadReader.InvalidPayload("checkpoint epoch mismatch")
              store.commit(docs, postings)
              replyTo ! BridgeReplyOk("bin:ws/CheckpointAck", checkpointBytes())

            case "bin:ws/Flush" =>
              reader.finish()
              val n = flushSegs()
              replyTo ! BridgeReplyOk("bin:ws/FlushAck", put32(n))

            case "bin:ws/Clear" =>
              reader.finish()
              val previous = store.segmentCount
              // Publish a durable empty snapshot before clearing memory. Older
              // immutable generations cannot resurrect data during recovery.
              store.commit(mutable.LongMap.empty[Doc], mutable.Map.empty[String, mutable.LongMap[Int]], resetEpoch = true)
              postings.clear(); docs.clear(); docLen.clear(); documentTerms.clear(); queries = 0L
              replyTo ! BridgeReplyOk("bin:ws/ClearAck", put32(previous))

            case "bin:ws/Healthz" =>
              reader.finish()
              val p = postings.valuesIterator.map(_.size.toLong).sum
              val json = s"""{"terms":${postings.size},"postings":$p,"queries":$queries,"docs":${docs.size},"segments":${store.segmentCount}}"""
              replyTo ! BridgeReplyOk("bin:ws/HealthzR", json.getBytes("UTF-8"))

            case "bin:ws/ListDocs" =>
              // 站点浏览：[offset u32|limit u32] → JSON（按 host 聚合 + URL 明细）
              val requestedOffset = reader.u32().toInt
              val limit = math.max(1L, math.min(reader.u32(), 200L)).toInt
              reader.finish()
              // host 聚合
              val byHost = mutable.Map.empty[String, mutable.ArrayBuffer[String]]
              docs.values.foreach { d =>
                val h = hostOf(d.url)
                byHost.getOrElseUpdate(h, mutable.ArrayBuffer.empty[String]) += d.url
              }
              val hosts = byHost.toVector.sortBy { case (host, urls) => (-urls.size, host) }
              val pageOff = pageOffset(requestedOffset, hosts.size, limit)
              val slice = hosts.slice(pageOff, pageOff + limit)
              val hostsJson = slice.map { case (h, urls) =>
                s"""{"host":"${esc(h)}","pages":${urls.size},"sample":"${esc(urls.sorted.head)}"}"""
              }.mkString("[", ",", "]")
              val json = s"""{"hosts":${hosts.size},"total_docs":${docs.size},"offset":$pageOff,"items":$hostsJson}"""
              replyTo ! BridgeReplyOk("bin:ws/ListDocsR", json.getBytes("UTF-8"))

            case "bin:ws/ListTerms" =>
              // 分词表浏览：[offset u32|limit u32] → JSON（按 df 降序）
              val requestedOffset = reader.u32().toInt
              val limit = math.max(1L, math.min(reader.u32(), 500L)).toInt
              reader.finish()
              val sorted = postings.toVector.map { case (t, ds) => (t, ds.size) }.sortBy { case (term, df) => (-df, term) }
              val pageOff = pageOffset(requestedOffset, sorted.size, limit)
              val slice = sorted.slice(pageOff, pageOff + limit)
              val items = slice.map { case (t, df) =>
                s"""{"term":"${esc(t)}","df":$df}"""
              }.mkString("[", ",", "]")
              val json = s"""{"total":${postings.size},"offset":$pageOff,"items":$items}"""
              replyTo ! BridgeReplyOk("bin:ws/ListTermsR", json.getBytes("UTF-8"))

            case _ => replyTo ! BridgeReplyErr(WireFrame.ErrCode.UnknownTypeKey, "unknown search request")
          }
        } catch {
          case e: SnapshotStore.HistoryPreparing => replyTo ! BridgeReplyErr(WireFrame.ErrCode.CodecError, e.getMessage)
          case e: PayloadReader.InvalidPayload => replyTo ! BridgeReplyErr(WireFrame.ErrCode.CodecError, e.getMessage)
          case e: IOException =>
            // Publication may already have happened if directory force failed.
            // Never continue acknowledging mutations against uncertain storage.
            storageFailed = true
            replyTo ! BridgeReplyErr(WireFrame.ErrCode.CodecError, "index storage failure: " + Option(e.getMessage).getOrElse("I/O error"))
        }
        Behaviors.same
      case _ => Behaviors.same
    }.receiveSignal {
      case (_, PostStop) => store.close(); Behaviors.same
    }
  }

  def segFiles(indexDir: File): Array[File] = SnapshotStore.files(indexDir).map(_._2).toArray
}
