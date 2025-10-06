package parrot.protocol.jvm

import akka.actor.typed.Behavior

/** B5 部署组件契约（DEV_09 §3.2 B5）：`DeployComponent{Jvm}` 的
  * main_class 需实现本 trait——网关经 child-first loader 载入后反射
  * 实例化，调 `behavior(ctx)` 取行为并 spawn 到本网关。
  *
  * 类身份约定：akka/scala/parrot 及 JDK 类一律走网关 classloader
  * （父优先），只有 artifact 自带的类 child-first——否则跨加载器的
  * BridgeAsk/BridgeReplyOk 类型身份会断裂。
  */
trait ComponentSpi {
  def behavior(ctx: ComponentContext): Behavior[Any]

  /** Drain 钩子（可选）：返回的 CompletionStage 完成即视为该实例可安全
    * 停止；deadline 内未完成 → aborted（实例保留运行但路由摘除——与 ray
    * 方言 `parrot_drain` 同语义）。默认 null = 立即排空。
    */
  def parrotDrain(): java.util.concurrent.CompletableFuture[_] = null
}

/** 部署上下文：实例路径（/user/{name}[-{i}]）、版本、可选 config overlay。 */
final case class ComponentContext(path: String, version: String, config: Option[Array[Byte]])
