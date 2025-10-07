/** websearch 检索服务（akka 方言——用户查询 Web 入口）。
  *
  * 五运行时分工（用户裁定）：本组件 = 检索/用户查询侧——
  *   - jieba-analysis 中文分词（查询切词——与 ray jieba / rust jieba-rs 同族）
  *   - 倒排索引 + BM25 打分（k1=1.2 b=0.75）
  *   - 段式落盘（data/index/seg-*.segment——重启回放，索引不丢）
  *   - doc 元数据表（docId → url/title）+ 正文摘要缓存
  *
  * 协议（bin:ws 前缀键空间）：
  *   bin:ws/IndexTerms [n u32][{len u32|term|docid u64|tf u32}...] → Ack [n u32]
  *   bin:ws/DocMeta    [docid u64|len u32|url|len u32|title|len u32|text(前2KB)] → Ack [1 u32]
  *   bin:ws/Search     [k u32|len u32|query utf8] → json [{doc,score,url,title,snippet}]
  *   bin:ws/Flush      [] → [segs u32]（段落盘命令——Rust 爬完调用）
  *   bin:ws/Healthz    [] → json {"terms","postings","queries","docs"}
  */
package websearch.search

import akka.actor.typed.Behavior
import akka.actor.typed.scaladsl.Behaviors
import com.huaban.analysis.jieba.JiebaSegmenter
import parrot.protocol.jvm.{ComponentContext, ComponentSpi}
import parrot.protocol.jvm.BridgeActor.{BridgeAsk, BridgeReplyOk}

import java.io.{DataInputStream, DataOutputStream, File, FileInputStream, FileOutputStream}
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

  private def u32(p: Array[Byte], off: Int): Int =
    (p(off).toInt & 0xFF) | ((p(off + 1).toInt & 0xFF) << 8) |
      ((p(off + 2).toInt & 0xFF) << 16) | ((p(off + 3).toInt & 0xFF) << 24)
  private def u64(p: Array[Byte], off: Int): Long = {
    var v = 0L; var b = 0
    while (b < 8) { v |= (p(off + b).toLong & 0xFF) << (8 * b); b += 1 }
    v
  }
  private def put32(n: Int): Array[Byte] = Array(
    (n & 0xFF).toByte, ((n >> 8) & 0xFF).toByte,
    ((n >> 16) & 0xFF).toByte, ((n >> 24) & 0xFF).toByte)

  /** jieba 查询切词（与索引侧同停用词表）。 */
  def tokenize(text: String): Vector[String] = {
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

  // ── 段文件格式（DataOutputStream 原生）──────────────────────────
  // [docs n][{docId u64|url utf|title utf|snippet utf}]
  // [terms n][{term utf|df i32|{docId u64|tf i32}*}]
  case class Doc(url: String, title: String, snippet: String)

  def writeSeg(f: File, docs: mutable.LongMap[Doc],
               postings: mutable.Map[String, mutable.LongMap[Int]]): Unit = {
    val tmp = new File(f.getParentFile, f.getName + ".tmp")
    val out = new DataOutputStream(new FileOutputStream(tmp))
    try {
      out.writeInt(docs.size)
      docs.foreach { case (id, d) =>
        out.writeLong(id); out.writeUTF(d.url); out.writeUTF(d.title); out.writeUTF(d.snippet)
      }
      out.writeInt(postings.size)
      postings.foreach { case (t, ds) =>
        out.writeUTF(t); out.writeInt(ds.size)
        ds.foreach { case (id, tf) => out.writeLong(id); out.writeInt(tf) }
      }
    } finally out.close()
    if (!tmp.renameTo(f)) { tmp.delete(); throw new IllegalStateException(s"rename fail $f") }
  }

  def readSeg(f: File): (mutable.LongMap[Doc], mutable.Map[String, mutable.LongMap[Int]]) = {
    val in = new DataInputStream(new FileInputStream(f))
    try {
      val docs = mutable.LongMap.empty[Doc]
      val postings = mutable.Map.empty[String, mutable.LongMap[Int]]
      val nd = in.readInt(); var i = 0
      while (i < nd) {
        docs.update(in.readLong(), Doc(in.readUTF(), in.readUTF(), in.readUTF())); i += 1
      }
      val nt = in.readInt(); i = 0
      while (i < nt) {
        val t = in.readUTF(); val df = in.readInt()
        val ds = mutable.LongMap.empty[Int]; var j = 0
        while (j < df) { ds.update(in.readLong(), in.readInt()); j += 1 }
        postings.update(t, ds); i += 1
      }
      (docs, postings)
    } finally in.close()
  }

  def searchActor(ctx: ComponentContext): Behavior[Any] = {
    // 数据目录：组件 config（TOML 片段）或环境变量 WS_DATA，缺省 ./data
    val dataDir = {
      val cfg = new String(ctx.config.getOrElse(Array.emptyByteArray), "UTF-8")
      val m = """(?m)^\s*data_dir\s*=\s*"([^"]+)"""".r.findFirstMatchIn(cfg)
      m.map(_.group(1)).getOrElse(sys.env.getOrElse("WS_DATA", "./data"))
    }
    val indexDir = new File(dataDir, "index"); indexDir.mkdirs()

    val postings = mutable.Map[String, mutable.LongMap[Int]]()
    val docs = mutable.LongMap.empty[Doc]
    var docLen = mutable.LongMap.empty[Int]   // docId → 词数（BM25 dl）
    var queries = 0L
    var nextSeg = segFiles(indexDir).length

    // 启动回放全部旧段（重启不丢——用户裁定 2）
    segFiles(indexDir).foreach { f =>
      val (d, p) = readSeg(f)
      d.foreach { case (id, doc) => docs.update(id, doc) }
      p.foreach { case (t, ds) =>
        val dst = postings.getOrElseUpdate(t, mutable.LongMap.empty[Int])
        ds.foreach { case (id, tf) => dst.update(id, tf); docLen.update(id, docLen.getOrElse(id, 0) + tf) }
      }
    }
    val replayed = segFiles(indexDir).length
    if (replayed > 0) println(s"[ws-search] 回放 $replayed 段（docs=${docs.size} terms=${postings.size}）")

    def flushSegs(): Int = {
      if (postings.nonEmpty) {
        val f = new File(indexDir, f"seg-$nextSeg%05d.segment")
        writeSeg(f, docs, postings)
        nextSeg += 1
        println(s"[ws-search] 段落盘 ${f.getName}（docs=${docs.size} terms=${postings.size}）")
        1
      } else 0
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
          ds.foreach { case (docId, tf) =>
            val dl = math.max(docLen.getOrElse(docId, tf).toDouble, 1.0)
            val denom = tf + k1 * (1 - b + b * dl / math.max(avg, 1.0))
            scores.update(docId, scores.getOrElse(docId, 0.0) + idf * tf * (k1 + 1) / denom)
          }
        }
      }
      scores.toVector.sortBy(-_._2).take(k)
    }

    def esc(s: String): String = s.replace("\\", "\\\\").replace("\"", "\\\"")
      .replace("\n", " ").replace("\r", " ")

    Behaviors.receiveMessage[Any] {
      case BridgeAsk(key, payload, replyTo) =>
        key match {
          case "bin:ws/IndexTerms" =>
            var off = 0
            val n = u32(payload, off); off += 4
            var i = 0
            while (i < n) {
              val tlen = u32(payload, off); off += 4
              val term = new String(payload, off, tlen, "UTF-8"); off += tlen
              val docId = u64(payload, off); off += 8
              val tf = u32(payload, off); off += 4
              postings.getOrElseUpdate(term, mutable.LongMap.empty[Int]).update(docId, tf)
              docLen.update(docId, docLen.getOrElse(docId, 0) + tf)
              i += 1
            }
            replyTo ! BridgeReplyOk("bin:ws/IndexAck", put32(n))

          case "bin:ws/DocMeta" =>
            // [docid u64|len u32 url|len u32 title|len u32 text]
            var off = 0
            val docId = u64(payload, off); off += 8
            val ul = u32(payload, off); off += 4
            val url = new String(payload, off, ul, "UTF-8"); off += ul
            val tl = u32(payload, off); off += 4
            val title = new String(payload, off, tl, "UTF-8"); off += tl
            val xl = u32(payload, off); off += 4
            val text = new String(payload, off, math.min(xl, 4096), "UTF-8")
            val snippet = text.replaceAll("\\s+", " ").take(280)
            docs.update(docId, Doc(url, title, snippet))
            replyTo ! BridgeReplyOk("bin:ws/DocAck", put32(1))

          case "bin:ws/Search" =>
            queries += 1
            var off = 0
            val k = u32(payload, off); off += 4
            val ql = u32(payload, off); off += 4
            val q = new String(payload, off, ql, "UTF-8")
            val terms = tokenize(q)
            val hits = bm25(terms, k)
            val json = hits.map { case (docId, score) =>
              docs.get(docId) match {
                case Some(d) =>
                  s"""{"doc":$docId,"score":${"%.2f".format(score)},"url":"${esc(d.url)}","title":"${esc(d.title)}","snippet":"${esc(d.snippet)}"}"""
                case None => s"""{"doc":$docId,"score":${"%.2f".format(score)}}"""
              }
            }.mkString("[", ",", "]")
            replyTo ! BridgeReplyOk("bin:ws/SearchResult", json.getBytes("UTF-8"))

          case "bin:ws/Flush" =>
            val n = flushSegs()
            replyTo ! BridgeReplyOk("bin:ws/FlushAck", put32(n))

          case "bin:ws/Clear" =>
            // 全新爬取：清内存索引 + 删磁盘段（Rust 主程序数据目录清理后调用）
            val segs = segFiles(indexDir)
            segs.foreach(_.delete())
            nextSeg = 0
            postings.clear(); docs.clear(); docLen.clear(); queries = 0L
            println(s"[ws-search] 索引已清空（删除 ${segs.length} 段）")
            replyTo ! BridgeReplyOk("bin:ws/ClearAck", put32(segs.length))

          case "bin:ws/Healthz" =>
            val p = postings.values.map(_.size).sum
            val json = s"""{"terms":${postings.size},"postings":$p,"queries":$queries,"docs":${docs.size},"segments":$nextSeg}"""
            replyTo ! BridgeReplyOk("bin:ws/HealthzR", json.getBytes("UTF-8"))

          case "bin:ws/ListDocs" =>
            // 站点浏览：[offset u32|limit u32] → JSON（按 host 聚合 + URL 明细）
            var off = 0
            val pageOff = u32(payload, off); off += 4
            val limit = math.max(1, math.min(u32(payload, off), 200)); off += 4
            // host 聚合
            val byHost = mutable.Map.empty[String, mutable.ArrayBuffer[String]]
            docs.values.foreach { d =>
              val h = hostOf(d.url)
              byHost.getOrElseUpdate(h, mutable.ArrayBuffer.empty[String]) += d.url
            }
            val hosts = byHost.toVector.sortBy(-_._2.size)
            val slice = hosts.slice(pageOff, pageOff + limit)
            val hostsJson = slice.map { case (h, urls) =>
              s"""{"host":"${esc(h)}","pages":${urls.size},"sample":"${esc(urls.sorted.head)}"}"""
            }.mkString("[", ",", "]")
            val json = s"""{"hosts":${hosts.size},"total_docs":${docs.size},"offset":$pageOff,"items":$hostsJson}"""
            replyTo ! BridgeReplyOk("bin:ws/ListDocsR", json.getBytes("UTF-8"))

          case "bin:ws/ListTerms" =>
            // 分词表浏览：[offset u32|limit u32] → JSON（按 df 降序）
            var off = 0
            val pageOff = u32(payload, off); off += 4
            val limit = math.max(1, math.min(u32(payload, off), 500)); off += 4
            val sorted = postings.toVector.map { case (t, ds) => (t, ds.size) }.sortBy(-_._2)
            val slice = sorted.slice(pageOff, pageOff + limit)
            val items = slice.map { case (t, df) =>
              s"""{"term":"${esc(t)}","df":$df}"""
            }.mkString("[", ",", "]")
            val json = s"""{"total":${postings.size},"offset":$pageOff,"items":$items}"""
            replyTo ! BridgeReplyOk("bin:ws/ListTermsR", json.getBytes("UTF-8"))

          case _ =>
        }
        Behaviors.same
      case _ => Behaviors.same
    }
  }

  def segFiles(indexDir: File): Array[File] =
    Option(indexDir.listFiles()).getOrElse(Array.empty)
      .filter(_.getName.endsWith(".segment")).sorted
}
