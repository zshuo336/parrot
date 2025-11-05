package parrot.protocol.jvm

import akka.actor.typed.Behavior
import akka.actor.typed.scaladsl.Behaviors
import BridgeActor._

/** Real child addressing: the component root cannot acknowledge a lifecycle hook. */
final class RustLifecycleFixture extends ComponentSpi {
  override def behavior(ctx: ComponentContext): Behavior[Any] = Behaviors.setup[Any] { actorContext =>
    // Only the child mutates/reads this state; root queries are forwarded to it.
    val child = actorContext.spawn(Behaviors.setup[Any] { _ =>
      var phases = Vector.empty[Byte]
      Behaviors.receiveMessage[Any] {
        case BridgeAsk("bin:app.hook#v1", payload, replyTo)
            if payload.length == 1 && payload(0) >= 0 && payload(0) <= 2 =>
          phases :+= payload(0)
          replyTo ! BridgeReplyOk("bin:app.hook#v1", payload)
          Behaviors.same
        case BridgeAsk("bin:test.LifecycleState", _, replyTo) =>
          replyTo ! BridgeReplyOk("bin:test.LifecycleState", phases.toArray)
          Behaviors.same
        case _ => Behaviors.same
      }
    }, "lifecycle")
    Behaviors.receiveMessage[Any] {
      case ask @ BridgeAsk("bin:test.LifecycleState", _, _) =>
        child ! ask
        Behaviors.same
      case BridgeAsk(_, _, replyTo) =>
        replyTo ! BridgeReplyOk("bin:test.LifecycleState", Array(99.toByte))
        Behaviors.same
      case _ => Behaviors.same
    }
  }
}
