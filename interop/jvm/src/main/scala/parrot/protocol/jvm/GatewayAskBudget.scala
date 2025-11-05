package parrot.protocol.jvm

/** Bounds requests awaiting a transport receipt, not application mailbox memory. */
final case class GatewayAskLimits(requests: Int = 1024, bytes: Long = 64L * 1024 * 1024,
                                  connectionRequests: Int = 128, connectionBytes: Long = 16L * 1024 * 1024) {
  require(requests > 0 && bytes > 0 && connectionRequests > 0 && connectionBytes > 0)
  require(connectionRequests <= requests && connectionBytes <= bytes)
}

/** Shared by all channels of one transport, including registration reconnects.
  * Disconnection fences replies but does not release permits early: the admitted
  * ASK still owns a promise/timer until its reply or the existing outer timeout.
  */
final class GatewayAskBudget private[jvm] (val limits: GatewayAskLimits = GatewayAskLimits()) extends AutoCloseable {
  import GatewayAskBudget._
  private[jvm] var ownerBound = false
  private var closed = false
  private var count = 0
  private var bytes = 0L
  private val pending = scala.collection.mutable.Set.empty[Connection#Ticket]
  private var retirement: Option[SystemRetirement#Registration] = None
  private def detachIfSettled(): Unit = if (closed && count == 0) {
    retirement.foreach(_.close()); retirement = None
  }
  private[jvm] def bindOwner(registration: SystemRetirement#Registration): Unit = synchronized {
    retirement = Some(registration)
    detachIfSettled()
  }
  private[jvm] def snapshot: (Int, Long) = synchronized { (count, bytes) }
  /** Stop admission and replies; keep owner retirement until all live waits end. */
  override def close(): Unit = synchronized { closed = true; detachIfSettled() }
  /** The owning ActorSystem is terminal: its scheduler/dispatcher may never run
    * another ASK callback. Fence those callbacks and retire the remaining ledger.
    */
  private[jvm] def ownerTerminated(): Unit = synchronized {
    closed = true
    pending.toVector.foreach(_.retire())
    detachIfSettled()
  }
  private[jvm] def connection(): Connection = new Connection

  final class Connection private[GatewayAskBudget] () {
    private var retired = false
    private val ids = scala.collection.mutable.Set.empty[Long]
    private var pendingBytes = 0L
    def close(): Unit = GatewayAskBudget.this.synchronized { retired = true }
    def owns(cid: Long): Boolean = GatewayAskBudget.this.synchronized { ids.contains(cid) }
    def acquire(cid: Long, weight: Long): Either[Rejection, Ticket] = GatewayAskBudget.this.synchronized {
      require(weight >= 0, "negative ASK weight")
      if (closed || retired) Left(Closed)
      else if (ids.contains(cid)) Left(Duplicate)
      else if (count >= limits.requests || ids.size >= limits.connectionRequests ||
        weight > limits.bytes - bytes || weight > limits.connectionBytes - pendingBytes) Left(Overloaded)
      else {
        ids.add(cid); count += 1; bytes += weight; pendingBytes += weight
        val ticket = new Ticket(cid, weight)
        pending.add(ticket)
        Right(ticket)
      }
    }

    final class Ticket private[Connection] (cid: Long, weight: Long) {
      private var done = false
      private def release(): Unit = {
        if (pending.remove(this)) {
          ids.remove(cid); count -= 1; bytes -= weight; pendingBytes -= weight
          detachIfSettled()
        }
      }
      private[GatewayAskBudget] def retire(): Unit = { done = true; release() }
      /** Queue the terminal receipt before freeing its correlation and capacity.
        * Never run response construction/Netty callbacks while holding the lock.
        */
      def complete(reply: => Unit): Unit = {
        val decision = GatewayAskBudget.this.synchronized {
          if (done) None else { done = true; Some(!closed && !retired) }
        }
        decision.foreach { deliver =>
          try { if (deliver) reply }
          finally GatewayAskBudget.this.synchronized {
            release()
          }
        }
      }
    }
  }
}

object GatewayAskBudget {
  /** Use this for directly embedded handlers; no separate initialization step. */
  def owned(system: akka.actor.typed.ActorSystem[_], limits: GatewayAskLimits = GatewayAskLimits()): GatewayAskBudget = {
    require(system != null, "ASK budget requires its owning ActorSystem")
    val budget = new GatewayAskBudget(limits)
    budget.ownerBound = true
    budget.bindOwner(SystemRetirement(system).register(() => budget.ownerTerminated()))
    budget
  }
  sealed trait Rejection
  case object Closed extends Rejection
  case object Duplicate extends Rejection
  case object Overloaded extends Rejection
}
