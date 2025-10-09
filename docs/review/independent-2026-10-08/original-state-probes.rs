use std::{sync::{Arc,Mutex,atomic::{AtomicU64,Ordering}}, time::Duration};
use parrot_api::{address::ActorRef, types::{ActorResult,BoxedActorRef,BoxedFuture,BoxedMessage}, errors::ActorError};
use parrot_remote::{LocalLookup,RemoteActorSystem,RemoteConfig};

#[derive(Debug,Clone,serde::Serialize,serde::Deserialize)]
struct Ping(u64);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:independent-review::Ping#v1",
        type_id: std::any::TypeId::of::<Ping>(),
        encode: |m: &BoxedMessage| parrot_api::message::serde_remote_serialize(m.downcast_ref::<Ping>().ok_or("Ping")?),
        decode: |b: &[u8]| { let p: Ping=parrot_api::message::serde_remote_deserialize(b)?; Ok(Box::new(p) as BoxedMessage) },
    }
}
#[derive(Debug,Clone)]
struct Sink { seen: Arc<Mutex<Vec<u64>>>, pending: bool, fail: bool }
#[async_trait::async_trait]
impl ActorRef for Sink {
 fn send<'a>(&'a self,m:BoxedMessage)->BoxedFuture<'a,ActorResult<BoxedMessage>> { Box::pin(async move { if self.pending { std::future::pending::<()>().await; } if self.fail { return Err(ActorError::InternalError("hook failed".into())); } Ok(m) }) }
 fn send_with_timeout<'a>(&'a self,m:BoxedMessage,_:Option<Duration>)->BoxedFuture<'a,ActorResult<BoxedMessage>> {self.send(m)}
 fn deliver<'a>(&'a self,m:BoxedMessage)->BoxedFuture<'a,ActorResult<()>> {Box::pin(async move {self.seen.lock().unwrap().push(m.downcast_ref::<Ping>().unwrap().0); Ok(())})}
 fn stop<'a>(&'a self)->BoxedFuture<'a,ActorResult<()>> {Box::pin(async {Ok(())})}
 fn path(&self)->String {"/user/probe".into()}
 fn is_alive<'a>(&'a self)->BoxedFuture<'a,bool> {Box::pin(async {true})}
 fn clone_boxed(&self)->BoxedActorRef {Box::new(self.clone())}
 fn as_any(&self)->&dyn std::any::Any {self}
}
struct Lookup(Sink);
#[async_trait::async_trait]
impl LocalLookup for Lookup {async fn lookup(&self,_:&str)->Option<BoxedActorRef> {Some(self.0.clone_boxed())}}
async fn pair(name:&str,pending:bool)->(Arc<RemoteActorSystem>,Arc<RemoteActorSystem>,Arc<Mutex<Vec<u64>>>) {
 let seen=Arc::new(Mutex::new(vec![]));
 let sink=Sink{seen:seen.clone(),pending,fail:false};
 let a=RemoteActorSystem::new(RemoteConfig::mem(format!("{name}-a")),Arc::new(Lookup(sink.clone()))).unwrap();
 let b=RemoteActorSystem::new(RemoteConfig::mem(format!("{name}-b")),Arc::new(Lookup(sink))).unwrap();
 a.start().await.unwrap();b.start().await.unwrap(); a.connect_mem_pair(&b).await.unwrap();
 tokio::time::sleep(Duration::from_millis(20)).await;(a,b,seen)
}
#[tokio::test]
async fn multiple_refs_drop_tells() {
 let(a,b,seen)=pair("refs",false).await;
 for n in 0..10 {a.remote_ref("parrot://refs-b/user/probe").unwrap().deliver(Box::new(Ping(n))).await.unwrap();}
 tokio::time::sleep(Duration::from_millis(200)).await;
 let got=seen.lock().unwrap().clone();eprintln!("PROBE multi_ref received={got:?}");assert_eq!(got,vec![0]);
 a.shutdown().await.unwrap();b.shutdown().await.unwrap();
}
#[tokio::test]
async fn externally_cancelled_ask_leaks_callback() {
 let(a,b,_)=pair("cancel",true).await;let r=a.remote_ref("parrot://cancel-b/user/probe").unwrap();
 assert!(tokio::time::timeout(Duration::from_millis(30),r.send(Box::new(Ping(1)))).await.is_err());
 eprintln!("PROBE external_timeout callbacks={}",a.callbacks.len());assert_eq!(a.callbacks.len(),1);
 let _=r.send_with_timeout(Box::new(Ping(2)),Some(Duration::from_millis(30))).await;
 assert_eq!(a.callbacks.len(),1);a.shutdown().await.unwrap();b.shutdown().await.unwrap();
}
#[test]
fn malformed_frame_panics() {
 let mut v=vec![0u8;14];v[0]=10;v[4]=1;v[5]=0x13;
 let out=std::panic::catch_unwind(||parrot_remote::frame::Frame::decode(&mut bytes::BytesMut::from(v.as_slice())));
 eprintln!("PROBE malformed panic={}",out.is_err());assert!(out.is_err());
}
#[test]
fn exhausted_hop_is_accepted() {
 use parrot_remote::frame::Frame;
 let mut f=Frame::tell_with_seq("/x","k",bytes::Bytes::new(),1);f.header.hop_count=8;f.header.hop_limit=8;
 let mut b=bytes::BytesMut::new();f.encode(&mut b).unwrap();assert!(Frame::decode(&mut b).unwrap().is_some());
 eprintln!("PROBE hop_count=hop_limit accepted");
}
struct Clock(AtomicU64);
impl parrot_remote::raft::Clock for Clock {fn now_ms(&self)->u64{self.0.load(Ordering::SeqCst)} fn election_timeout_ms(&self)->u64{100} fn heartbeat_ms(&self)->u64{10}}
#[test]
fn four_node_raft_commits_with_two_replicas() {
 use parrot_remote::raft::{RaftNode,RaftRpc,Role};
 let clock=Arc::new(Clock(AtomicU64::new(0)));let mut n=RaftNode::new("a",vec!["b".into(),"c".into(),"d".into()],clock.clone());
 clock.0.store(1000,Ordering::SeqCst);n.tick();
 for p in ["b","c"]{n.step(p.into(),RaftRpc::RequestVoteResp{term:n.current_term,vote_granted:true});}
 assert_eq!(n.role,Role::Leader);n.propose(vec![42]).unwrap();assert_eq!(n.commit_index,0);
 n.step("b".into(),RaftRpc::AppendEntriesResp{term:n.current_term,success:true,match_index:1});
 eprintln!("PROBE raft 2/4 replicas commit={}",n.commit_index);assert_eq!(n.commit_index,1);
}
#[test]
fn twin_mapping_is_not_injective() {
 let twin=federation_lab::twin::DigitalTwin::build(Default::default());
 let a=twin.expected_of("parrot://fed/c0/n0").unwrap();let b=twin.expected_of("parrot://fed/c0/n256").unwrap();
 eprintln!("PROBE twin n0={a:?} n256={b:?}");assert_eq!(a,b);
}
struct Spawner(Arc<Mutex<Vec<String>>>);
impl parrot_app::assemble::ParrotSpawner for Spawner {
 fn spawn_props(&self,_:&str,paths:&[String])->Result<Vec<BoxedActorRef>,parrot_app::assemble::DeployError>{
 self.0.lock().unwrap().extend_from_slice(paths);Ok(vec![Box::new(Sink{seen:Default::default(),pending:false,fail:true})]) }
 fn stop_paths(&self,paths:&[String])->Result<(),parrot_app::assemble::DeployError>{self.0.lock().unwrap().retain(|p|!paths.contains(p));Ok(())}
}
struct Gw;
impl parrot_app::assemble::GatewayFactory for Gw {
 fn start_gateway(&self,_:&parrot_app::manifest::ComponentSpec,_:&[String])->Result<Vec<BoxedActorRef>,parrot_app::assemble::DeployError>{unreachable!()}
 fn stop_gateway(&self,_:&parrot_app::manifest::ComponentSpec)->Result<(),parrot_app::assemble::DeployError>{Ok(())}
}
#[tokio::test]
async fn failing_start_hook_leaves_component_running(){
 use parrot_app::{manifest::AppManifest,planner::{plan,LocalTopology},assemble::{AssemblingContext,LocalDeployer}};
 let m=AppManifest::from_toml("name='probe'\nversion='1.0.0'\n[[components]]\nname='broken'\nengine='parrot'\nartifact={Props={factory='x'}}\n[components.hooks]\non_start='/ready'\n").unwrap();
 let plan=plan(&m,&LocalTopology).unwrap();let alive=Arc::new(Mutex::new(vec![]));let sp=Spawner(alive.clone());let gw=Gw;
 let deploy=LocalDeployer{topology:&LocalTopology,gateway_factory:&gw,parrot_spawner:&sp};let mut ctx=AssemblingContext::new();
 assert!(ctx.assemble(&plan,&deploy,&m,&Default::default()).await.is_err());ctx.teardown(&deploy).await.unwrap();
 eprintln!("PROBE rollback alive={:?} started={:?}",alive.lock().unwrap(),ctx.started());assert_eq!(*alive.lock().unwrap(),vec!["/user/broken"]);
}

use parrot::{thread::{system::ThreadActorSystem,context::ThreadContext,config::{ThreadActorSystemConfig,ThreadActorConfig,SchedulingMode}}};
use parrot_api::actor::{Actor,ActorState,EmptyConfig};
struct Forwarder(Option<BoxedActorRef>);
impl Actor for Forwarder {
 type Config=EmptyConfig;type Context=ThreadContext<Self>;
 fn receive_message<'a>(&'a mut self,m:BoxedMessage,_:&'a mut Self::Context)->BoxedFuture<'a,ActorResult<BoxedMessage>> {
 Box::pin(async move {match &self.0 {None=>Ok(m),Some(r)=>r.send_with_timeout(m,Some(Duration::from_millis(250))).await}})
 }
 fn state(&self)->ActorState{ActorState::Running}
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn single_shared_worker_blocks_acyclic_ask(){
 let ts=ThreadActorSystem::shared(ThreadActorSystemConfig{shared_pool_size:1,shared_burst_workers_max:0,..Default::default()});
 let b=ts.spawn_at(Forwarder(None),"/user/b",None,Default::default()).await.unwrap();
 let a=ts.spawn_at(Forwarder(Some(Box::new(b))),"/user/a",None,Default::default()).await.unwrap();
 let start=std::time::Instant::now();let result=a.send(Box::new(Ping(1))).await;
 eprintln!("PROBE shared worker=1 A->B result={result:?} elapsed={:?}",start.elapsed());assert!(result.is_err());
 ts.shutdown_internal().await.unwrap();
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn same_shard_blocks_acyclic_ask(){
 let ts=ThreadActorSystem::shared(Default::default());
 let conf=ThreadActorConfig{scheduling_mode:Some(SchedulingMode::Sharded{affinity_key:"same".into()}),..Default::default()};
 let b=ts.spawn_at(Forwarder(None),"/user/sb",None,conf.clone()).await.unwrap();
 let a=ts.spawn_at(Forwarder(Some(Box::new(b))),"/user/sa",None,conf).await.unwrap();
 let start=std::time::Instant::now();let result=a.send(Box::new(Ping(1))).await;
 eprintln!("PROBE same shard A->B result={result:?} elapsed={:?}",start.elapsed());assert!(result.is_err());
 ts.shutdown_internal().await.unwrap();
}
impl parrot_api::message::Message for Ping {type Result=u64;}
struct TypedEcho;
impl parrot_api::typed::TypedReceive<Ping> for TypedEcho {
 fn receive_typed<'a>(&'a mut self,p:Ping)->BoxedFuture<'a,ActorResult<u64>>{Box::pin(async move{Ok(p.0)})}
}
#[tokio::test]
async fn typed_actor_survives_system_shutdown_and_duplicates_path(){
 use parrot::thread::typed::SingleVariant;
 let ts=ThreadActorSystem::shared(Default::default());
 let a=ts.spawn_typed_single::<TypedEcho,Ping>(TypedEcho,"/user/typed").await.unwrap();
 let b=ts.spawn_typed_single::<TypedEcho,Ping>(TypedEcho,"/user/typed").await.unwrap();
 assert_eq!(ts.actor_count(),0);
 ts.shutdown_internal().await.unwrap();
 let got=a.ask(SingleVariant::wrap(Ping(42))).await.unwrap();
 let got_b=b.ask(SingleVariant::wrap(Ping(43))).await.unwrap();
 eprintln!("PROBE typed duplicate_path accepted, registry_count=0, after_shutdown replies={got},{got_b}");assert_eq!(got,42);
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn tell_helper_reorders_single_caller(){
 use parrot_api::address::ActorRefExt;
 let seen=Arc::new(Mutex::new(vec![]));let r=Sink{seen:seen.clone(),pending:false,fail:false};
 for n in 0..20000{r.tell(Ping(n));}
 for _ in 0..100{if seen.lock().unwrap().len()==20000{break}tokio::time::sleep(Duration::from_millis(10)).await;}
 let v=seen.lock().unwrap();let inversions=v.windows(2).filter(|w|w[0]>w[1]).count();
 eprintln!("PROBE tell_helper count={} inversions={inversions}",v.len());assert_eq!(v.len(),20000);assert!(inversions>0);
}
#[test]
fn wal_recovery_replays_acknowledged_record(){
 use parrot_remote::durable::{Wal,WalRecord};
 let dir=std::env::temp_dir().join(format!("parrot-review-wal-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
 let w=Wal::open(&dir).unwrap();w.append(WalRecord{endpoint:"e".into(),sender:"s".into(),seq:1,path:"/x".into(),type_key:"k".into(),payload:bytes::Bytes::new()}).unwrap();
 w.ack("e","s",1).unwrap();w.sync().unwrap();assert_eq!(w.replay("e").len(),0);drop(w);
 let w=Wal::open(&dir).unwrap();let n=w.replay("e").len();eprintln!("PROBE WAL acknowledged replay after reopen={n}");assert_eq!(n,1);
}
