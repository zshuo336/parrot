package parrot.protocol.jvm

import akka.actor.typed.{ActorSystem, Extension, ExtensionId}
import scala.concurrent.ExecutionContext
import scala.util.control.NonFatal

/** One Future callback per ActorSystem; resource registrations can be removed.
  * Registry locks never surround cleanup, including registration after retirement.
  */
private[jvm] object SystemRetirement extends ExtensionId[SystemRetirement] {
  override def createExtension(system: ActorSystem[_]): SystemRetirement = {
    val termination = system.whenTerminated
    val registry = new SystemRetirement(ExecutionContext.parasitic.reportFailure, () => termination.isCompleted)
    termination.onComplete(_ => registry.retire())(ExecutionContext.parasitic)
    registry
  }
}

private[jvm] final class SystemRetirement(reportFailure: Throwable => Unit,
                                         isTerminated: () => Boolean = () => false) extends Extension {
  private var closed = false
  private var callbacks = Map.empty[Object, () => Unit]
  private[jvm] def registeredCount: Int = synchronized { callbacks.size }

  final class Registration private[SystemRetirement] (key: Object) extends AutoCloseable {
    // The token stores only its registry/key; close removes its cleanup capture.
    override def close(): Unit = SystemRetirement.this.synchronized { callbacks -= key }
  }
  def register(cleanup: () => Unit): Registration = {
    // A waiter may observe the terminal Future before its callbacks have run.
    // New resources must already be fenced at that observable boundary.
    if (isTerminated()) retire()
    val key = new Object
    val added = synchronized {
      if (closed) false else { callbacks = callbacks.updated(key, cleanup); true }
    }
    if (!added) invoke(cleanup)
    new Registration(key)
  }
  private def invoke(cleanup: () => Unit): Unit =
    try cleanup()
    catch { case NonFatal(error) => reportFailure(error) }

  def retire(): Unit = {
    val owned = synchronized {
      closed = true
      val result = callbacks.values.toVector
      callbacks = Map.empty
      result
    }
    owned.foreach(invoke)
  }
}
