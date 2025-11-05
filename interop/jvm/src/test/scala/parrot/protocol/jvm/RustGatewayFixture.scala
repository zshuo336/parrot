package parrot.protocol.jvm

import akka.actor.typed.Behavior
import akka.actor.typed.scaladsl.Behaviors
import BridgeActor._

/** Loaded by the Rust TCP integration test through the real component executor. */
final class RustGatewayFixture extends ComponentSpi {
  override def behavior(ctx: ComponentContext): Behavior[Any] =
    Behaviors.receiveMessage[Any] {
      case BridgeAsk(_, payload, replyTo) =>
        replyTo ! BridgeReplyOk("bin:parrot.test.ConfiguredEcho#v1",
          ctx.config.getOrElse(Array.emptyByteArray) ++ payload)
        Behaviors.same
      case _ => Behaviors.same
    }
}
