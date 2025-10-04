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
      replyTo: ActorRef[BridgeReply]
  ) extends BridgeMsg
  final case class Tell(akkaPath: String, typeKey: String, payload: Array[Byte]) extends BridgeMsg
  final case class StopAkka(akkaPath: String, replyTo: ActorRef[StopAck]) extends BridgeMsg
  final case class PipeBack(to: ActorRef[BridgeReply], reply: BridgeReply) extends BridgeMsg

  sealed trait BridgeReply
  final case class Replied(cid: Long, typeKey: String, payload: Array[Byte]) extends BridgeReply
  final case class ReplyErr(cid: Long, code: Int, detail: String) extends BridgeReply

  sealed trait StopAck
  case object StopUnsupported extends StopAck

  def apply(resolve: String => Option[ActorRef[Any]]): Behavior[BridgeMsg] =
    Behaviors.setup { ctx =>
      implicit val timeout: Timeout = Timeout(5.seconds)
      implicit val sch = ctx.system.scheduler

      Behaviors.receiveMessage {
        case Ask(cid, path, key, payload, replyTo) =>
          resolve(path) match {
            case Some(target) =>
              import akka.actor.typed.scaladsl.AskPattern._
              val fut: Future[Any] = target.ask[Any](ref => BridgeAsk(key, payload, ref))
              ctx.pipeToSelf(fut) {
                case scala.util.Success(BridgeReplyOk(rkey, rpayload)) =>
                  PipeBack(replyTo, Replied(cid, rkey, rpayload))
                case scala.util.Success(other) =>
                  PipeBack(replyTo, ReplyErr(cid, WireFrame.ErrCode.CodecError, s"unexpected: $other"))
                case scala.util.Failure(ex) =>
                  PipeBack(replyTo, ReplyErr(cid, WireFrame.ErrCode.Timeout, Option(ex.getMessage).getOrElse("ask failed")))
              }
              Behaviors.same
            case None =>
              replyTo ! ReplyErr(cid, WireFrame.ErrCode.ActorNotFound, path)
              Behaviors.same
          }
        case Tell(path, key, payload) =>
          resolve(path).foreach(_ ! BridgeTell(key, payload))
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
  final case class BridgeAsk(typeKey: String, payload: Array[Byte], replyTo: ActorRef[Any])
  final case class BridgeTell(typeKey: String, payload: Array[Byte])
  final case class BridgeReplyOk(typeKey: String, payload: Array[Byte])
}
