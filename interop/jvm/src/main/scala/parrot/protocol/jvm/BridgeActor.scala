package parrot.protocol.jvm

import akka.actor.typed.{ActorRef, Behavior}
import akka.actor.typed.scaladsl.Behaviors
import akka.util.Timeout
import scala.concurrent.Future
import scala.concurrent.duration._

/** 桥语义（DEV_02 §6.2）：
  * ASK → AskPattern → REPLY/REPLY_ERR；TELL → ref ! ；STOP → 不支持
  * （akka 无外部 stop 语义——I4 缺口保留，回 Unsupported）。
  *
  * 路径映射：parrot://{gw}/jvm/user/{akkaPath} ↔ akka user guardian 下路径。
  */
object BridgeActor {

  sealed trait BridgeMsg
  final case class Ask(
      cid: Long,
      akkaPath: String,
      typeKey: String,
      payload: Array[Byte],
      replyTo: ActorRef[BridgeReply],
      traceParent: Option[CausalTrace.Parent]
  ) extends BridgeMsg with CausalTrace.Carrier {
    require(traceParent != null && traceParent.forall(_ != null), "null parent option or identity")
    def this(cid: Long, akkaPath: String, typeKey: String, payload: Array[Byte], replyTo: ActorRef[BridgeReply]) =
      this(cid, akkaPath, typeKey, payload, replyTo, CausalTrace.current)
    def copy(cid: Long, akkaPath: String, typeKey: String, payload: Array[Byte], replyTo: ActorRef[BridgeReply]): Ask =
      new Ask(cid, akkaPath, typeKey, payload, replyTo, traceParent)
    def copy(cid: Long = this.cid, akkaPath: String = this.akkaPath, typeKey: String = this.typeKey,
             payload: Array[Byte] = this.payload, replyTo: ActorRef[BridgeReply] = this.replyTo,
             traceParent: Option[CausalTrace.Parent] = this.traceParent): Ask =
      new Ask(cid, akkaPath, typeKey, payload, replyTo, traceParent)
  }
  object Ask extends scala.runtime.AbstractFunction5[Long, String, String, Array[Byte], ActorRef[BridgeReply], Ask] {
    def apply(cid: Long, akkaPath: String, typeKey: String, payload: Array[Byte], replyTo: ActorRef[BridgeReply]): Ask =
      new Ask(cid, akkaPath, typeKey, payload, replyTo, CausalTrace.current)
    def unapply(value: Ask): Option[(Long, String, String, Array[Byte], ActorRef[BridgeReply])] =
      Option(value).map(v => (v.cid, v.akkaPath, v.typeKey, v.payload, v.replyTo))
  }
  final case class Tell(akkaPath: String, typeKey: String, payload: Array[Byte], traceParent: Option[CausalTrace.Parent])
      extends BridgeMsg with CausalTrace.Carrier {
    require(traceParent != null && traceParent.forall(_ != null), "null parent option or identity")
    def this(akkaPath: String, typeKey: String, payload: Array[Byte]) = this(akkaPath, typeKey, payload, CausalTrace.current)
    def copy(akkaPath: String, typeKey: String, payload: Array[Byte]): Tell = new Tell(akkaPath, typeKey, payload, traceParent)
    def copy(akkaPath: String = this.akkaPath, typeKey: String = this.typeKey, payload: Array[Byte] = this.payload,
             traceParent: Option[CausalTrace.Parent] = this.traceParent): Tell = new Tell(akkaPath, typeKey, payload, traceParent)
  }
  object Tell extends scala.runtime.AbstractFunction3[String, String, Array[Byte], Tell] {
    def apply(akkaPath: String, typeKey: String, payload: Array[Byte]): Tell = new Tell(akkaPath, typeKey, payload, CausalTrace.current)
    def unapply(value: Tell): Option[(String, String, Array[Byte])] = Option(value).map(v => (v.akkaPath, v.typeKey, v.payload))
  }
  final case class StopAkka(akkaPath: String, replyTo: ActorRef[StopAck]) extends BridgeMsg
  final case class PipeBack(to: ActorRef[BridgeReply], reply: BridgeReply) extends BridgeMsg

  sealed trait BridgeReply
  final case class Replied(cid: Long, typeKey: String, payload: Array[Byte]) extends BridgeReply
  final case class ReplyErr(cid: Long, code: Int, detail: String) extends BridgeReply

  sealed trait StopAck
  case object StopUnsupported extends StopAck

  private final case class ResolvedPath(path: String, target: Option[ActorRef[Any]]) extends BridgeMsg

  def apply(
      resolve: String => Option[ActorRef[Any]],
      resolveChild: String => Future[Option[ActorRef[Any]]] = _ => Future.successful(None)
  ): Behavior[BridgeMsg] =
    Behaviors.setup { ctx =>
      implicit val timeout: Timeout = Timeout(5.seconds)
      implicit val sch = ctx.system.scheduler

      val pending = scala.collection.mutable.Map.empty[String, Vector[BridgeMsg]]
      var pendingCount = 0
      var pendingBytes = 0L
      def weight(message: BridgeMsg): Long = message match {
        case request @ Ask(_, path, key, payload, _) => payload.length.toLong + path.length * 2L + key.length * 2L + request.traceParent.fold(0)(_ => 25)
        case request @ Tell(path, key, payload) => payload.length.toLong + path.length * 2L + key.length * 2L + request.traceParent.fold(0)(_ => 25)
        case _ => 0L
      }
      def dispatch(message: BridgeMsg, target: Option[ActorRef[Any]]): Unit = message match {
        case request @ Ask(cid, path, key, payload, replyTo) => target match {
          case Some(actor) =>
            import akka.actor.typed.scaladsl.AskPattern._
            val fut: Future[Any] = actor.ask[Any](ref => BridgeAsk(key, payload, ref, request.traceParent))
            ctx.pipeToSelf(fut) {
              case scala.util.Success(BridgeReplyOk(rkey, rpayload)) => PipeBack(replyTo, Replied(cid, rkey, rpayload))
              case scala.util.Success(BridgeReplyErr(code, detail)) => PipeBack(replyTo, ReplyErr(cid, code, detail.take(1024)))
              case scala.util.Success(other) => PipeBack(replyTo, ReplyErr(cid, WireFrame.ErrCode.CodecError, s"unexpected: $other"))
              case scala.util.Failure(ex) => PipeBack(replyTo, ReplyErr(cid, WireFrame.ErrCode.Timeout, Option(ex.getMessage).getOrElse("ask failed")))
            }
          case None => replyTo ! ReplyErr(cid, WireFrame.ErrCode.ActorNotFound, path)
        }
        case request @ Tell(_, key, payload) => target.foreach(_ ! BridgeTell(key, payload, request.traceParent))
        case _ => ()
      }
      def route(message: BridgeMsg, path: String): Unit = resolve(path) match {
        case Some(actor) => dispatch(message, Some(actor))
        case None if !path.contains("/") => dispatch(message, None)
        case None =>
          val bytes = weight(message)
          if (pendingCount >= 1024 || pendingBytes + bytes > 16L * 1024 * 1024) {
            message match {
              case Ask(cid, _, _, _, replyTo) => replyTo ! ReplyErr(cid, WireFrame.ErrCode.Overloaded, "child resolution capacity exhausted")
              case _ => ()
            }
          } else {
            val first = !pending.contains(path)
            pending.update(path, pending.getOrElse(path, Vector.empty) :+ message)
            pendingCount += 1
            pendingBytes += bytes
            if (first) {
              val future = scala.util.Try(resolveChild(path)).fold(Future.failed, identity)
              ctx.pipeToSelf(future) {
                case scala.util.Success(target) => ResolvedPath(path, target)
                case scala.util.Failure(_) => ResolvedPath(path, None)
              }
            }
          }
      }
      Behaviors.receiveMessage {
        case request: Ask =>
          route(request, request.akkaPath)
          Behaviors.same
        case request: Tell =>
          route(request, request.akkaPath)
          Behaviors.same
        case ResolvedPath(path, target) =>
          pending.remove(path).foreach { requests =>
            pendingCount -= requests.size
            pendingBytes -= requests.map(weight).sum
            requests.foreach(request => dispatch(request, target))
          }
          Behaviors.same
        case StopAkka(_, replyTo) =>
          replyTo ! StopUnsupported
          Behaviors.same
        case PipeBack(to, reply) =>
          to ! reply
          Behaviors.same
      }
    }

  /** 桥协议消息（目标 akka actor 需处理的形态——网关侧 echo/cpu 均实现）。 */
  final case class BridgeAsk(typeKey: String, payload: Array[Byte], replyTo: ActorRef[Any], traceParent: Option[CausalTrace.Parent])
      extends CausalTrace.Carrier {
    require(traceParent != null && traceParent.forall(_ != null), "null parent option or identity")
    def this(typeKey: String, payload: Array[Byte], replyTo: ActorRef[Any]) = this(typeKey, payload, replyTo, CausalTrace.current)
    def copy(typeKey: String, payload: Array[Byte], replyTo: ActorRef[Any]): BridgeAsk = new BridgeAsk(typeKey, payload, replyTo, traceParent)
    def copy(typeKey: String = this.typeKey, payload: Array[Byte] = this.payload, replyTo: ActorRef[Any] = this.replyTo,
             traceParent: Option[CausalTrace.Parent] = this.traceParent): BridgeAsk = new BridgeAsk(typeKey, payload, replyTo, traceParent)
  }
  object BridgeAsk extends scala.runtime.AbstractFunction3[String, Array[Byte], ActorRef[Any], BridgeAsk] {
    def apply(typeKey: String, payload: Array[Byte], replyTo: ActorRef[Any]): BridgeAsk = new BridgeAsk(typeKey, payload, replyTo, CausalTrace.current)
    def unapply(value: BridgeAsk): Option[(String, Array[Byte], ActorRef[Any])] = Option(value).map(v => (v.typeKey, v.payload, v.replyTo))
  }
  final case class BridgeTell(typeKey: String, payload: Array[Byte], traceParent: Option[CausalTrace.Parent]) extends CausalTrace.Carrier {
    require(traceParent != null && traceParent.forall(_ != null), "null parent option or identity")
    def this(typeKey: String, payload: Array[Byte]) = this(typeKey, payload, CausalTrace.current)
    def copy(typeKey: String, payload: Array[Byte]): BridgeTell = new BridgeTell(typeKey, payload, traceParent)
    def copy(typeKey: String = this.typeKey, payload: Array[Byte] = this.payload,
             traceParent: Option[CausalTrace.Parent] = this.traceParent): BridgeTell = new BridgeTell(typeKey, payload, traceParent)
  }
  object BridgeTell extends scala.runtime.AbstractFunction2[String, Array[Byte], BridgeTell] {
    def apply(typeKey: String, payload: Array[Byte]): BridgeTell = new BridgeTell(typeKey, payload, CausalTrace.current)
    def unapply(value: BridgeTell): Option[(String, Array[Byte])] = Option(value).map(v => (v.typeKey, v.payload))
  }
  /** Explicit application rejection; forwarded as Wire REPLY_ERR with the original correlation ID. */
  final case class BridgeReplyErr(code: Int, detail: String)
  final case class BridgeReplyOk(typeKey: String, payload: Array[Byte])
}
