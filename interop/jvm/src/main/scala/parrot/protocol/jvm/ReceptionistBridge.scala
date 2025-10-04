package parrot.protocol.jvm

import akka.actor.typed.receptionist.{Receptionist, ServiceKey}
import akka.actor.typed.scaladsl.Behaviors
import akka.actor.typed.{ActorRef, Behavior}

/** akka receptionist ↔ parrot ReceptionistEvent 双向桥（DEV_02 §6.2）。
  *
  * parrot key "{scope}/{name}" 直接作为 akka ServiceKey id（非法字符规则两端
  * 一致：空格/`*`/`#` 拒绝）。
  *
  * listing diff → parrot 事件：
  *   新增 ActorRef   → Registered(key, "parrot://{nodeId}/jvm/user/{actorName}")
  *   消失 ActorRef   → Unregistered(key, 同路径)
  *
  * 订阅即回放快照（初始 listing）——与 parrot subscribe 的快照+续流语义对齐。
  */
object ReceptionistBridge {

  sealed trait ParrotEvent
  final case class Registered(key: String, remotePath: String)   extends ParrotEvent
  final case class Unregistered(key: String, remotePath: String) extends ParrotEvent

  /** parrot key 校验（与 parrot-api ReceptionistKey::new 同规则）。 */
  def validateKey(key: String): Boolean =
    key.nonEmpty && !key.contains(' ') && !key.contains('*') && !key.contains('#')

  /** akka ServiceKey[Any]——任意 Behavior[Any] actor 可注册。 */
  def toServiceKey(parrotKey: String): ServiceKey[Any] = ServiceKey[Any](parrotKey)

  /** akka actor ref → parrot 远程路径（网关路径文法 /jvm/user/{name}）。 */
  def toRemotePath(nodeId: String, ref: ActorRef[Any]): String = {
    val path = ref.path.toString // akka://parrot-gw/user/echo
    val name = path.substring(path.lastIndexOf('/') + 1)
    s"parrot://$nodeId/jvm/user/$name"
  }

  /** 桥行为：订阅 listing → diff → emit parrot 事件。 */
  def apply(
      nodeId: String,
      parrotKey: String,
      emit: ParrotEvent => Unit
  ): Behavior[Receptionist.Listing] =
    Behaviors.setup { ctx =>
      require(validateKey(parrotKey), s"illegal parrot receptionist key: $parrotKey")
      val svc = toServiceKey(parrotKey)
      var last: Set[ActorRef[Any]] = Set.empty
      ctx.system.receptionist ! Receptionist.Subscribe(svc, ctx.self)
      Behaviors.receiveMessage { listing =>
        val cur: Set[ActorRef[Any]] = listing.allServiceInstances(svc).toSet
        // diff：新增 → Registered；消失 → Unregistered
        (cur -- last).foreach { ref =>
          emit(Registered(parrotKey, toRemotePath(nodeId, ref)))
        }
        (last -- cur).foreach { ref =>
          emit(Unregistered(parrotKey, toRemotePath(nodeId, ref)))
        }
        last = cur
        Behaviors.same
      }
    }
}
