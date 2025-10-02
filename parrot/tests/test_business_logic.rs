//! 业务逻辑正确性专项套件（第十轮）。
//!
//! 与 C 系列（引擎语义层：恰好一次/FIFO/回复路由）互补，本套件验证
//! **业务逻辑层**：actor 收到消息后，其内部业务状态变迁、计算结果、
//! 副作用是否正确 —— 即"处理对了"，而不只是"收到了"。
//!
//! 方法论：**模型对照（model-based testing）** —— 测试侧用独立实现的
//! 业务期望模型重放同样规则，最终与 actor 实际状态精确比对；并发场景
//! 下校验业务守恒不变量（总额守恒、库存无泄漏）。
//!
//! B1 银行账户：并发存取后余额守恒 + 拒绝透支且状态不被污染
//! B2 聚合统计：10k 值并发提交，sum/n/min/max 与数学期望精确一致
//! B3 订单状态机：合法变迁生效 + 非法变迁拒绝且状态不变
//! B4 三 actor 业务流水线：下单→库存→支付，含**失败补偿回滚**，
//!    最终三方状态一致（库存零泄漏、资金守恒）
//! B5 业务计算服务：字符串规范化结果与本地参考实现逐条一致
//! B6 错误路径业务语义：业务校验失败返回业务错误 + 状态不变
//! B7 副作用审计流：处理 actor 的每个业务事件被审计 actor 完整记录

mod engine_stress_common;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorSystemConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef as ActorRefTrait;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

async fn setup(name: &str) -> (ParrotActorSystem, Arc<ThreadActorSystem>) {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    parrot.register_thread_system(name.into(), ts.clone(), true).await.unwrap();
    (parrot, ts)
}

async fn ask_boxed<A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static>(
    r: &parrot::thread::address::ThreadActorRef<A>,
    m: BoxedMessage,
) -> BoxedMessage {
    r.ask(m).await.expect("ask must succeed")
}

// ===========================================================================
// B1 银行账户：有状态业务 actor + 并发守恒不变量
// ===========================================================================

/// 业务回复：携带业务结果或业务错误（区别于引擎错误）
#[derive(Debug, Clone, PartialEq)]
enum AccountReply {
    Ok(u64),              // 新余额
    InsufficientFunds,    // 业务校验失败
}

struct Deposit { amount: u64 }
struct Withdraw { amount: u64 }
struct GetBalance;

struct AccountActor { balance: u64 }

impl Actor for AccountActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        // ---- 业务逻辑（在 actor 内执行，这是被验证对象）----
        let reply: AccountReply = if let Some(d) = m.downcast_ref::<Deposit>() {
            self.balance += d.amount; // 存款无条件成功
            AccountReply::Ok(self.balance)
        } else if let Some(w) = m.downcast_ref::<Withdraw>() {
            if w.amount <= self.balance {
                self.balance -= w.amount;
                AccountReply::Ok(self.balance)
            } else {
                // 业务规则：拒绝透支，状态不变
                AccountReply::InsufficientFunds
            }
        } else if m.downcast_ref::<GetBalance>().is_some() {
            AccountReply::Ok(self.balance)
        } else {
            return Box::pin(async { Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into())) });
        };
        Box::pin(async move { Ok(Box::new(reply) as BoxedMessage) })
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None // 强制走 receive_message 执行业务逻辑
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[test]
fn b1_bank_account_business_invariants() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(8).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("biz1").await;
        const INITIAL: u64 = 1_000;
        let acc = ts
            .spawn_at::<AccountActor>(AccountActor { balance: INITIAL }, "/biz/account", None, Default::default())
            .await
            .unwrap();

        // 并发业务流：8 客户端 × 各 200 笔（存/取交替，取款额会超余额触发拒绝）
        let ops_per_client = 200u64;
        let mut hs = Vec::new();
        for c in 0..8u64 {
            let a = acc.clone();
            hs.push(tokio::spawn(async move {
                // 返回该客户端执行的 (deposit_total, withdraw_ok_total, withdraw_rejected_count)
                let (mut dep, mut wd, mut rej) = (0u64, 0u64, 0u64);
                for k in 0..ops_per_client {
                    if k % 2 == 0 {
                        let amount = 50 + (c * 7 + k) % 100;
                        let r = *ask_boxed(&a, Box::new(Deposit { amount })).await.downcast::<AccountReply>().unwrap();
                        match r {
                            AccountReply::Ok(_) => dep += amount,
                            _ => panic!("deposit must never fail"),
                        }
                    } else {
                        // 故意制造超额取款（触发业务拒绝路径）
                        let amount = if k % 8 == 7 { 50_000 } else { 30 + (c * 3 + k) % 90 };
                        let r = *ask_boxed(&a, Box::new(Withdraw { amount })).await.downcast::<AccountReply>().unwrap();
                        match r {
                            AccountReply::Ok(_) => wd += amount,
                            AccountReply::InsufficientFunds => rej += 1,
                        }
                    }
                }
                (dep, wd, rej)
            }));
        }
        let mut total_dep = 0u64;
        let mut total_wd = 0u64;
        let mut total_rej = 0u64;
        for h in hs {
            let (d, w, r) = h.await.unwrap();
            total_dep += d;
            total_wd += w;
            total_rej += r;
        }

        // ---- 业务守恒不变量（模型对照）----
        let final_reply = *ask_boxed(&acc, Box::new(GetBalance)).await.downcast::<AccountReply>().unwrap();
        let actual = match final_reply { AccountReply::Ok(b) => b, _ => panic!("balance query failed") };
        let expected = INITIAL + total_dep - total_wd;
        assert_eq!(actual, expected,
            "BUSINESS INVARIANT VIOLATED: balance must equal initial + deposits - successful withdrawals");
        assert!(total_rej > 0, "test must exercise the insufficient-funds path (got {} rejections)", total_rej);
        println!(
            "[B1] bank: initial={} +dep={} -wd={} => balance={} ✓ | overdraft rejected {} times, state unpolluted ✓",
            INITIAL, total_dep, total_wd, actual, total_rej
        );
        let _ = ts.shutdown_internal().await;
    });
}

// ===========================================================================
// B2 聚合统计：业务计算精确性
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
struct StatsSnapshot { n: u64, sum: i64, min: i64, max: i64 }

struct SubmitValue { v: i64 }
struct GetStats;

struct StatsActor { n: u64, sum: i64, min: i64, max: i64 }

impl Actor for StatsActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if let Some(s) = m.downcast_ref::<SubmitValue>() {
            // 业务逻辑：聚合更新
            self.n += 1;
            self.sum += s.v;
            if self.n == 1 || s.v < self.min { self.min = s.v; }
            if self.n == 1 || s.v > self.max { self.max = s.v; }
            let n = self.n;
            Box::pin(async move { Ok(Box::new(n) as BoxedMessage) })
        } else if m.downcast_ref::<GetStats>().is_some() {
            let snap = StatsSnapshot { n: self.n, sum: self.sum, min: self.min, max: self.max };
            Box::pin(async move { Ok(Box::new(snap) as BoxedMessage) })
        } else {
            Box::pin(async { Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into())) })
        }
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[test]
fn b2_aggregation_exactness() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(8).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("biz2").await;
        let stats = ts
            .spawn_at::<StatsActor>(StatsActor { n: 0, sum: 0, min: 0, max: 0 }, "/biz/stats", None, Default::default())
            .await
            .unwrap();

        // 并发提交 10000 个值（8 线程 × 1250）
        const WORKERS: u64 = 8;
        const PER: u64 = 1_250;
        let mut hs = Vec::new();
        for w in 0..WORKERS {
            let s = stats.clone();
            hs.push(tokio::spawn(async move {
                let mut acks = 0u64;
                for k in 0..PER {
                    let v: i64 = ((w * PER + k) as i64 % 2001) - 1000; // [-1000, 1000]
                    let n = *ask_boxed(&s, Box::new(SubmitValue { v })).await.downcast::<u64>().unwrap();
                    acks += n; // 消费 ack（单调递增序列号）
                }
                let _ = acks;
            }));
        }
        for h in hs { h.await.unwrap(); }

        // ---- 模型对照：数学期望 ----
        let total = WORKERS * PER;
        let expected_sum: i64 = (0..total).map(|i| (i as i64 % 2001) - 1000).sum();
        let expected_min: i64 = (0..total).map(|i| (i as i64 % 2001) - 1000).min().unwrap();
        let expected_max: i64 = (0..total).map(|i| (i as i64 % 2001) - 1000).max().unwrap();
        let snap = *ask_boxed(&stats, Box::new(GetStats)).await.downcast::<StatsSnapshot>().unwrap();
        assert_eq!(snap.n, total, "count must be exactly-once");
        assert_eq!(snap.sum, expected_sum, "sum must match model");
        assert_eq!(snap.min, expected_min, "min must match model");
        assert_eq!(snap.max, expected_max, "max must match model");
        println!("[B2] stats over {} concurrent values: n={} sum={} min={} max={} all match model ✓", total, snap.n, snap.sum, snap.min, snap.max);
        let _ = ts.shutdown_internal().await;
    });
}

// ===========================================================================
// B3 订单状态机：合法/非法变迁
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
enum OrderState { Created, Paid, Shipped, Delivered, Cancelled }

#[derive(Debug, Clone, PartialEq)]
enum TransitionReply { Ok(OrderState), Invalid { from: OrderState, tried: &'static str } }

struct EvPay; struct EvShip; struct EvDeliver; struct EvCancel; struct GetOrderState;

struct OrderMachine { state: OrderState }

impl OrderMachine {
    /// 业务规则表：合法变迁
    fn transition(&mut self, ev: &'static str) -> TransitionReply {
        let ok = matches!(
            (self.state, ev),
            (OrderState::Created, "pay")
                | (OrderState::Paid, "ship")
                | (OrderState::Shipped, "deliver")
                | (OrderState::Created, "cancel")
                | (OrderState::Paid, "cancel")
        );
        if !ok {
            return TransitionReply::Invalid { from: self.state, tried: ev };
        }
        self.state = match (self.state, ev) {
            (OrderState::Created, "pay") => OrderState::Paid,
            (OrderState::Paid, "ship") => OrderState::Shipped,
            (OrderState::Shipped, "deliver") => OrderState::Delivered,
            _ => OrderState::Cancelled,
        };
        TransitionReply::Ok(self.state)
    }
}

impl Actor for OrderMachine {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        // 初始业务状态
        self.state = OrderState::Created;
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let reply = if m.downcast_ref::<EvPay>().is_some() { self.transition("pay") }
        else if m.downcast_ref::<EvShip>().is_some() { self.transition("ship") }
        else if m.downcast_ref::<EvDeliver>().is_some() { self.transition("deliver") }
        else if m.downcast_ref::<EvCancel>().is_some() { self.transition("cancel") }
        else if m.downcast_ref::<GetOrderState>().is_some() {
            return Box::pin(async move { Ok(Box::new(self.state) as BoxedMessage) });
        } else {
            return Box::pin(async { Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into())) });
        };
        Box::pin(async move { Ok(Box::new(reply) as BoxedMessage) })
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[test]
fn b3_order_state_machine() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("biz3").await;

        // 完整合法链：Created→Paid→Shipped→Delivered
        let m1 = ts.spawn_at::<OrderMachine>(OrderMachine { state: OrderState::Created }, "/biz/o1", None, Default::default()).await.unwrap();
        let r1 = *ask_boxed(&m1, Box::new(EvPay)).await.downcast::<TransitionReply>().unwrap();
        assert_eq!(r1, TransitionReply::Ok(OrderState::Paid), "pay from Created must succeed");
        let r2 = *ask_boxed(&m1, Box::new(EvShip)).await.downcast::<TransitionReply>().unwrap();
        assert_eq!(r2, TransitionReply::Ok(OrderState::Shipped));
        let r3 = *ask_boxed(&m1, Box::new(EvDeliver)).await.downcast::<TransitionReply>().unwrap();
        assert_eq!(r3, TransitionReply::Ok(OrderState::Delivered));
        // 终态后一切变迁非法
        let r4 = *ask_boxed(&m1, Box::new(EvCancel)).await.downcast::<TransitionReply>().unwrap();
        assert_eq!(r4, TransitionReply::Invalid { from: OrderState::Delivered, tried: "cancel" }, "cancel after Delivered must be rejected");
        let s = *ask_boxed(&m1, Box::new(GetOrderState)).await.downcast::<OrderState>().unwrap();
        assert_eq!(s, OrderState::Delivered, "rejected transition must NOT change state");

        // 非法跳变：Created→Ship 必须被拒
        let m2 = ts.spawn_at::<OrderMachine>(OrderMachine { state: OrderState::Created }, "/biz/o2", None, Default::default()).await.unwrap();
        let r5 = *ask_boxed(&m2, Box::new(EvShip)).await.downcast::<TransitionReply>().unwrap();
        assert_eq!(r5, TransitionReply::Invalid { from: OrderState::Created, tried: "ship" }, "ship from Created must be rejected");
        let s2 = *ask_boxed(&m2, Box::new(GetOrderState)).await.downcast::<OrderState>().unwrap();
        assert_eq!(s2, OrderState::Created, "state must remain Created");

        // 分支链：Created→Paid→Cancelled，Cancelled 为终态
        let m3 = ts.spawn_at::<OrderMachine>(OrderMachine { state: OrderState::Created }, "/biz/o3", None, Default::default()).await.unwrap();
        let _ = ask_boxed(&m3, Box::new(EvPay)).await;
        let r6 = *ask_boxed(&m3, Box::new(EvCancel)).await.downcast::<TransitionReply>().unwrap();
        assert_eq!(r6, TransitionReply::Ok(OrderState::Cancelled));
        let r7 = *ask_boxed(&m3, Box::new(EvShip)).await.downcast::<TransitionReply>().unwrap();
        assert_eq!(r7, TransitionReply::Invalid { from: OrderState::Cancelled, tried: "ship" });

        println!("[B3] order machine: legal chains applied ✓ illegal transitions rejected with state intact ✓");
        let _ = ts.shutdown_internal().await;
    });
}

// ===========================================================================
// B4 三 actor 业务流水线：下单→库存→支付，失败补偿回滚，三方状态一致
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
enum ReserveReply { Granted { remaining: u64 }, OutOfStock { available: u64 } }
struct Reserve { item: String, qty: u64 }
struct Restock { item: String, qty: u64 }
struct GetStock { item: String }

struct InventoryActor { stock: HashMap<String, u64> }

impl Actor for InventoryActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        if self.stock.is_empty() {
            self.stock.insert("widget".into(), 100);
            self.stock.insert("gadget".into(), 30);
        }
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let reply = if let Some(r) = m.downcast_ref::<Reserve>() {
            let avail = *self.stock.get(&r.item).unwrap_or(&0);
            if r.qty <= avail {
                *self.stock.get_mut(&r.item).unwrap() -= r.qty;
                ReserveReply::Granted { remaining: avail - r.qty }
            } else {
                ReserveReply::OutOfStock { available: avail }
            }
        } else if let Some(rs) = m.downcast_ref::<Restock>() {
            // 补偿事务：支付失败时归还库存
            *self.stock.entry(rs.item.clone()).or_insert(0) += rs.qty;
            ReserveReply::Granted { remaining: 0 }
        } else {
            return Box::pin(async { Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into())) });
        };
        Box::pin(async move { Ok(Box::new(reply) as BoxedMessage) })
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[derive(Debug, Clone, PartialEq)]
enum ChargeReply { Approved { remaining_funds: u64 }, Declined { required: u64, available: u64 } }
struct Charge { amount: u64 }
struct GetLedger;

struct PaymentActor { funds: u64, charged_total: u64, tx_count: u64 }

impl Actor for PaymentActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if let Some(c) = m.downcast_ref::<Charge>() {
            if c.amount <= self.funds {
                self.funds -= c.amount;
                self.charged_total += c.amount;
                self.tx_count += 1;
                let r = ChargeReply::Approved { remaining_funds: self.funds };
                Box::pin(async move { Ok(Box::new(r) as BoxedMessage) })
            } else {
                let r = ChargeReply::Declined { required: c.amount, available: self.funds };
                Box::pin(async move { Ok(Box::new(r) as BoxedMessage) })
            }
        } else if m.downcast_ref::<GetLedger>().is_some() {
            // 业务快照：(funds, charged_total, tx_count)
            let snap = (self.funds, self.charged_total, self.tx_count);
            Box::pin(async move { Ok(Box::new(snap) as BoxedMessage) })
        } else {
            Box::pin(async { Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into())) })
        }
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[derive(Debug, Clone, PartialEq)]
enum OrderOutcome { Confirmed, NoStock, NoFunds }

/// 带处理序号的业务回执：seq 由 orchestrator 单调分配 = 真实处理顺序。
/// 模型对照必须按此顺序重放（并发到达顺序不确定，处理顺序才是不变量）。
#[derive(Debug, Clone, PartialEq)]
struct OrderReceipt { seq: u64, outcome: OrderOutcome }
struct PlaceOrder { order_id: u64, item: String, qty: u64, unit_price: u64 }
struct GetOrderCounts;

/// 编排 actor：真正的多 actor 业务流程（Saga 模式 + 补偿）
struct OrderOrchestrator {
    inventory: parrot::thread::address::ThreadActorRef<InventoryActor>,
    payment: parrot::thread::address::ThreadActorRef<PaymentActor>,
    confirmed: u64,
    failed_no_stock: u64,
    failed_no_funds: u64,
    confirmed_qty: HashMap<String, u64>,
    confirmed_amount: u64,
    seq: u64,
}

impl Actor for OrderOrchestrator {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let Some(o) = m.downcast_ref::<PlaceOrder>() else {
                if m.downcast_ref::<GetOrderCounts>().is_some() {
                    return Ok(Box::new((self.confirmed, self.failed_no_stock, self.failed_no_funds)) as BoxedMessage);
                }
                return Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into()));
            };
            self.seq += 1;
            let seq = self.seq;
            // ---- 业务流程（Saga）：预留库存 → 扣款；扣款失败补偿归还 ----
            let reserve = *self
                .inventory
                .ask(Box::new(Reserve { item: o.item.clone(), qty: o.qty }))
                .await?
                .downcast::<ReserveReply>()
                .unwrap();
            match reserve {
                ReserveReply::OutOfStock { .. } => {
                    self.failed_no_stock += 1;
                    return Ok(Box::new(OrderReceipt { seq, outcome: OrderOutcome::NoStock }) as BoxedMessage);
                }
                ReserveReply::Granted { .. } => {}
            }
            let amount = o.qty * o.unit_price;
            let charge = *self
                .payment
                .ask(Box::new(Charge { amount }))
                .await?
                .downcast::<ChargeReply>()
                .unwrap();
            match charge {
                ChargeReply::Approved { .. } => {
                    self.confirmed += 1;
                    *self.confirmed_qty.entry(o.item.clone()).or_insert(0) += o.qty;
                    self.confirmed_amount += amount;
                    Ok(Box::new(OrderReceipt { seq, outcome: OrderOutcome::Confirmed }) as BoxedMessage)
                }
                ChargeReply::Declined { .. } => {
                    // ---- 补偿事务：归还库存（业务正确性关键）----
                    let _ = self
                        .inventory
                        .ask(Box::new(Restock { item: o.item.clone(), qty: o.qty }))
                        .await?;
                    self.failed_no_funds += 1;
                    Ok(Box::new(OrderReceipt { seq, outcome: OrderOutcome::NoFunds }) as BoxedMessage)
                }
            }
        })
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[test]
fn b4_saga_pipeline_consistency() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(8).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("biz4").await;

        let inv_init = HashMap::from([("widget".to_string(), 100u64), ("gadget".to_string(), 30u64)]);
        let inv = ts
            .spawn_at::<InventoryActor>(InventoryActor { stock: inv_init.clone() }, "/biz/inv", None, Default::default())
            .await
            .unwrap();
        let PAY_INIT: u64 = 900;
        let pay = ts
            .spawn_at::<PaymentActor>(PaymentActor { funds: PAY_INIT, charged_total: 0, tx_count: 0 }, "/biz/pay", None, Default::default())
            .await
            .unwrap();
        let orch = ts
            .spawn_at::<OrderOrchestrator>(
                OrderOrchestrator {
                    inventory: inv.clone(),
                    payment: pay.clone(),
                    confirmed: 0,
                    failed_no_stock: 0,
                    failed_no_funds: 0,
                    confirmed_qty: HashMap::new(),
                    confirmed_amount: 0,
                    seq: 0,
                },
                "/biz/orch",
                None,
                Default::default(),
            )
            .await
            .unwrap();

        // 200 笔订单：混合库存不足（gadget 超量）、资金不足（后期大单）、成功
        let mut hs = Vec::new();
        for i in 0..200u64 {
            let o = orch.clone();
            hs.push(tokio::spawn(async move {
                let (item, qty, price) = match i % 5 {
                    0 => ("widget", 3u64, 10u64),   // 30/单
                    1 => ("gadget", 1u64, 20u64),    // 20/单（库存限量 30）
                    2 => ("widget", 2u64, 15u64),   // 30/单
                    3 => ("gadget", 5u64, 9u64),    // 45/单
                    _ => ("widget", 1u64, 25u64),   // 25/单
                };
                let r = *ask_boxed(&o, Box::new(PlaceOrder { order_id: i, item: item.into(), qty, unit_price: price }))
                    .await
                    .downcast::<OrderReceipt>()
                    .unwrap();
                (item, qty, price, r)
            }));
        }
        let mut outcomes = Vec::new();
        for h in hs {
            outcomes.push(h.await.unwrap());
        }
        // ---- 业务守恒不变量（对并发交错不敏感的强校验）----
        // 每单是原子的 reserve→charge→(失败补偿 restock) 序列，orchestrator
        // 单线程处理保证与某串行执行等价；以下不变量对到达顺序均不敏感：
        //   ① 计数守恒 ② 资金守恒 ③ 库存守恒（补偿零泄漏）④ 金额上限
        let confirmed_amount_from_receipts: u64 = outcomes
            .iter()
            .filter(|(_, _, _, r)| r.outcome == OrderOutcome::Confirmed)
            .map(|(_, qty, price, _)| *qty * *price)
            .sum();
        let confirmed_qty_widget: u64 = outcomes
            .iter()
            .filter(|(item, _, _, r)| *item == "widget" && r.outcome == OrderOutcome::Confirmed)
            .map(|(_, qty, _, _)| *qty)
            .sum();
        let confirmed_qty_gadget: u64 = outcomes
            .iter()
            .filter(|(item, _, _, r)| *item == "gadget" && r.outcome == OrderOutcome::Confirmed)
            .map(|(_, qty, _, _)| *qty)
            .sum();
        assert!(
            confirmed_amount_from_receipts <= PAY_INIT,
            "conservation impossible: confirmed {} > initial funds {}",
            confirmed_amount_from_receipts, PAY_INIT
        );

        let (confirmed, no_stock, no_funds) =
            *ask_boxed(&orch, Box::new(GetOrderCounts)).await.downcast::<(u64, u64, u64)>().unwrap();
        let model_confirmed = outcomes.iter().filter(|(_, _, _, r)| r.outcome == OrderOutcome::Confirmed).count() as u64;
        let model_no_stock = outcomes.iter().filter(|(_, _, _, r)| r.outcome == OrderOutcome::NoStock).count() as u64;
        let model_no_funds = outcomes.iter().filter(|(_, _, _, r)| r.outcome == OrderOutcome::NoFunds).count() as u64;
        assert_eq!(confirmed + no_stock + no_funds, 200, "every order must have exactly one outcome");
        assert_eq!((confirmed, no_stock, no_funds), (model_confirmed, model_no_stock, model_no_funds),
            "orchestrator counters must equal receipts (每单恰好一个回执且计数一致)");

        // 库存一致性：用 Reserve(qty=0) 试探读取（Granted.remaining = 当前可用量）
        // 支付台账 + 订单计数核算（库存守恒：初始 - 确认量 = 当前，因失败单已补偿）
        let (funds, charged_total, tx_count) = *ask_boxed(&pay, Box::new(GetLedger)).await.downcast::<(u64, u64, u64)>().unwrap();
        assert_eq!(charged_total, confirmed_amount_from_receipts, "资金守恒: ledger total must equal Σ confirmed receipts");
        assert_eq!(funds, PAY_INIT - confirmed_amount_from_receipts, "资金守恒: funds = initial - confirmed");
        assert_eq!(tx_count, confirmed, "one charge per confirmed order");
        // 库存守恒：/widget 最终库存 = 100 - Σconfirmed widget qty
        let w_conf = confirmed_qty_widget;
        let g_conf = confirmed_qty_gadget;
        // 通过 Reserve 试探 0 件读取剩余量（Granted.remaining 返回可用量，qty=0 恒 Granted）
        let w_left = match *ask_boxed(&inv, Box::new(Reserve { item: "widget".into(), qty: 0 })).await.downcast::<ReserveReply>().unwrap() {
            ReserveReply::Granted { remaining } => remaining,
            _ => panic!(),
        };
        let g_left = match *ask_boxed(&inv, Box::new(Reserve { item: "gadget".into(), qty: 0 })).await.downcast::<ReserveReply>().unwrap() {
            ReserveReply::Granted { remaining } => remaining,
            _ => panic!(),
        };
        assert_eq!(w_left, 100 - w_conf, "widget stock conservation (compensation must have zero leakage)");
        assert_eq!(g_left, 30 - g_conf, "gadget stock conservation");
        assert!(no_funds > 0, "test must exercise compensation path");
        println!(
            "[B4] saga over 200 concurrent orders: confirmed={} no_stock={} no_funds(compensated)={} | funds {}→{} ✓ stock widget {}/{}, gadget {}/{} zero leakage ✓",
            confirmed, no_stock, no_funds, PAY_INIT, funds, w_left, 100 - w_conf, g_left, 30 - g_conf
        );
        let _ = ts.shutdown_internal().await;
    });
}

// ===========================================================================
// B5 业务计算服务：结果与本地参考逐条一致
// ===========================================================================

/// 业务：文本规范化（小写、按非字母切词、去重保序、逗号连接）
struct NormalizeText { raw: String }
struct NormalizeActor;

fn normalize_ref(raw: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let lowered = raw.to_lowercase();
    for w in lowered.split(|c: char| !c.is_alphanumeric()) {
        if !w.is_empty() && seen.insert(w.to_string()) {
            out.push(w);
        }
    }
    out.join(",")
}

impl Actor for NormalizeActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let Some(t) = m.downcast_ref::<NormalizeText>() else {
            return Box::pin(async { Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into())) });
        };
        // actor 内独立实现同一业务规则（不同写法，验证语义等价而非复制粘贴）
        let mut words = Vec::new();
        let mut cur = String::new();
        for ch in t.raw.chars() {
            let lc = ch.to_lowercase().next().unwrap_or(ch);
            if lc.is_alphanumeric() {
                cur.push(lc);
            } else if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() { words.push(cur); }
        let mut deduped: Vec<String> = Vec::new();
        for w in words {
            if !deduped.contains(&w) { deduped.push(w); }
        }
        let result = deduped.join(",");
        Box::pin(async move { Ok(Box::new(result) as BoxedMessage) })
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[test]
fn b5_business_computation_correctness() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("biz5").await;
        let svc = ts.spawn_at::<NormalizeActor>(NormalizeActor, "/biz/norm", None, Default::default()).await.unwrap();

        let cases = vec![
            "Hello World hello WORLD",
            "  leading and trailing spaces  ",
            "MiXeD CaSe WoRdS",
            "punct,uation;and:marks!!!",
            "duplicates duplicates duplicates unique",
            "unicode: héllo wörld héllo",
            "a-b-c-d a-b",
            "",
            "42 numbers 42 and 7",
            "tabs\tand\nnewlines\r\nhere",
        ];
        for (i, case) in cases.iter().enumerate() {
            let got = *ask_boxed(&svc, Box::new(NormalizeText { raw: (*case).to_string() })).await.downcast::<String>().unwrap();
            let expect = normalize_ref(case);
            assert_eq!(got, expect, "case #{} ({:?}): actor result must match reference", i, case);
        }
        println!("[B5] text normalization: 10 tricky cases (case/dup/unicode/empty/punct) all match reference implementation ✓");
        let _ = ts.shutdown_internal().await;
    });
}

// ===========================================================================
// B6 错误路径业务语义（与 B1 呼应但聚焦错误契约）
// ===========================================================================

#[test]
fn b6_error_contract() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("biz6").await;
        let acc = ts
            .spawn_at::<AccountActor>(AccountActor { balance: 100 }, "/biz/eacc", None, Default::default())
            .await
            .unwrap();

        // 业务错误必须是"业务回复"（可 downcast 的枚举），不是引擎异常
        let r = *ask_boxed(&acc, Box::new(Withdraw { amount: 999 })).await.downcast::<AccountReply>().unwrap();
        assert_eq!(r, AccountReply::InsufficientFunds, "overdraft must yield business error reply");

        // 错误后状态零污染
        let r2 = *ask_boxed(&acc, Box::new(Withdraw { amount: 100 })).await.downcast::<AccountReply>().unwrap();
        assert_eq!(r2, AccountReply::Ok(0), "exact-balance withdraw must succeed after a rejected one");

        // 未知消息类型必须是引擎级错误（区分业务失败 vs 协议失败）
        let r3 = acc.ask(Box::new(42u8) as BoxedMessage).await;
        assert!(r3.is_err(), "unknown message type must fail at engine level");
        println!("[B6] error contract: business failure = typed reply, protocol failure = engine error, state unpolluted ✓");
        let _ = ts.shutdown_internal().await;
    });
}

// ===========================================================================
// B7 副作用审计流：业务事件被完整、有序记录
// ===========================================================================

struct BizEvent { id: u64, kind: &'static str }
struct RecordEvent { id: u64, kind: &'static str }
struct GetAuditLog;

struct AuditActor { log: Vec<(u64, &'static str)> }

impl Actor for AuditActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if let Some(r) = m.downcast_ref::<RecordEvent>() {
            self.log.push((r.id, r.kind));
            Box::pin(async move { Ok(Box::new(()) as BoxedMessage) })
        } else if m.downcast_ref::<GetAuditLog>().is_some() {
            let snapshot = self.log.clone();
            Box::pin(async move { Ok(Box::new(snapshot) as BoxedMessage) })
        } else {
            Box::pin(async { Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into())) })
        }
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

struct BusinessProcessor {
    audit: parrot::thread::address::ThreadActorRef<AuditActor>,
}

impl Actor for BusinessProcessor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(&'a mut self, m: BoxedMessage, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let Some(e) = m.downcast_ref::<BizEvent>() else {
                return Err(parrot_api::errors::ActorError::MessageHandlingError("unknown".into()));
            };
            // 业务处理 + 副作用：必须先完成业务决策再审计（或反之），二者一致
            let outcome: &'static str = match e.kind {
                "credit" => "ledger_updated",
                "debit" => "ledger_updated",
                _ => "noop",
            };
            // 审计记录必须含原始事件与业务结果
            let _ = self
                .audit
                .ask(Box::new(RecordEvent { id: e.id, kind: outcome }))
                .await?;
            Ok(Box::new(outcome) as BoxedMessage)
        })
    }
    fn receive_message_with_engine<'a>(&'a mut self, _m: BoxedMessage, _c: &'a mut Self::Context, _e: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
        None
    }
    fn state(&self) -> ActorState { ActorState::Running }
}

#[test]
fn b7_side_effect_audit_trail() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("biz7").await;
        let audit = ts.spawn_at::<AuditActor>(AuditActor { log: Vec::new() }, "/biz/audit", None, Default::default()).await.unwrap();
        let proc = ts
            .spawn_at::<BusinessProcessor>(BusinessProcessor { audit: audit.clone() }, "/biz/proc", None, Default::default())
            .await
            .unwrap();

        // 500 个业务事件（单生产者：顺序可预测）
        let mut expected = Vec::new();
        for i in 0..500u64 {
            let kind = match i % 3 {
                0 => "credit",
                1 => "debit",
                _ => "query",
            };
            let reply = *ask_boxed(&proc, Box::new(BizEvent { id: i, kind })).await.downcast::<&'static str>().unwrap();
            let expect_outcome = match kind { "credit" | "debit" => "ledger_updated", _ => "noop" };
            assert_eq!(reply, expect_outcome, "business decision for event {} ({}) wrong", i, kind);
            expected.push((i, expect_outcome));
        }

        // 副作用完整性：审计日志与业务结果序列逐一对应
        let log = *ask_boxed(&audit, Box::new(GetAuditLog)).await.downcast::<Vec<(u64, &'static str)>>().unwrap();
        assert_eq!(log.len(), 500, "every processed event must be audited exactly once");
        assert_eq!(log, expected, "audit trail must match business outcome sequence in order");
        println!("[B7] audit trail: 500 events, every business decision recorded exactly-once, in order ✓");
        let _ = ts.shutdown_internal().await;
    });
}
