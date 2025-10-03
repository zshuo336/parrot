//! POC 1：derive 宏反向依赖解耦（评测报告 D2 债务）。
//!
//! 现状问题：`ParrotActor` 宏生成代码硬编码 `parrot::actix::*` 符号，
//! "规范不依赖实现"在符号级被击穿。
//!
//! 解法（注册点反转）：
//!   1. 规范层（parrot_api）定义"引擎注册点"：ErasedActor / EngineRuntime
//!   2. 宏生成代码**只引用规范层符号**（::parrot_api::...），零引擎符号
//!   3. 实现层（parrot::actix / parrot::thread / 第三方）各自实现
//!      EngineRuntime 并注册；用户代码经 `extern crate x as parrot_api`
//!      或直接依赖规范 crate 完成绑定
//!
//! POC 用 `extern crate api_neutral as parrot_api;` 证明符号面完全中立。

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput};

/// 规范侧 derive：生成"引擎中立"的 actor 胶水。
/// 生成物：
///   - `impl ErasedActor for &mut Self` 风格的 receive 桥（要求 Self 实现
///     规范层 `ActorBehaviour`，这是唯一的类型约束来源）
///   - `spawn_on(engine, path)`：经 EngineRuntime trait 对象 spawn
#[proc_macro_derive(ParrotActorNeutral)]
pub fn derive_parrot_actor_neutral(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let expanded = quote! {
        impl ::parrot_api::ErasedActor for #name {
            fn receive(
                &mut self,
                msg: ::parrot_api::BoxedMessage,
            ) -> ::parrot_api::BoxedResult {
                ::parrot_api::ActorBehaviour::receive(self, msg)
            }
        }

        impl #name {
            /// 引擎中立 spawn：任意实现 EngineRuntime 的引擎均可承载。
            pub fn spawn_on<E: ::parrot_api::EngineRuntime>(
                self,
                engine: &E,
                path: &str,
            ) -> Result<::parrot_api::BoxedActorRef, String> {
                engine.spawn_erased(Box::new(self), path)
            }
        }
    };
    TokenStream::from(expanded)
}
