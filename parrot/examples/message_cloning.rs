use parrot::thread::{BoxedMessageExt, make_cloneable};
use parrot_api::types::BoxedMessage;
use std::time::Duration;

#[derive(Clone, Debug)]
struct CustomMessage {
    id: u32,
    payload: String,
    timestamp: u64,
}

fn main() {
    // 创建各种类型的消息
    let primitive_message: BoxedMessage = Box::new(42);
    let string_message: BoxedMessage = Box::new("Hello, Parrot!".to_string());
    
    let custom_message = CustomMessage {
        id: 1001,
        payload: "Important data".to_string(),
        timestamp: 1622548800,
    };
    let complex_message: BoxedMessage = Box::new(custom_message);
    
    // 使用扩展trait尝试克隆消息
    println!("克隆各种类型的消息:");
    
    // 克隆原始类型消息
    if let Some(cloned) = primitive_message.try_clone() {
        if let Some(value) = cloned.downcast_ref::<i32>() {
            println!("成功克隆原始类型消息: {}", value);
        }
    } else {
        println!("无法克隆原始类型消息");
    }
    
    // 克隆字符串消息
    if let Some(cloned) = string_message.try_clone() {
        if let Some(value) = cloned.downcast_ref::<String>() {
            println!("成功克隆字符串消息: {}", value);
        }
    } else {
        println!("无法克隆字符串消息");
    }
    
    // 克隆自定义复杂类型消息
    if let Some(cloned) = complex_message.try_clone() {
        if let Some(value) = cloned.downcast_ref::<CustomMessage>() {
            println!("成功克隆自定义消息: ID={}, 负载={}, 时间戳={}", 
                     value.id, value.payload, value.timestamp);
        }
    } else {
        println!("无法克隆自定义消息");
    }
    
    // 使用克隆或者提供默认值
    println!("\n使用clone_or_default:");
    
    // 创建一个无法克隆的类型
    struct NonCloneable(i32);
    let non_cloneable: BoxedMessage = Box::new(NonCloneable(999));
    
    let default_value = || Box::new("默认值") as BoxedMessage;
    let result = non_cloneable.clone_or_default(default_value);
    
    if let Some(value) = result.downcast_ref::<String>() {
        println!("使用默认值: {}", value);
    }
    
    // 演示make_cloneable
    println!("\n使用make_cloneable助手函数:");
    
    let another_message = CustomMessage {
        id: 2002,
        payload: "Another important data".to_string(),
        timestamp: 1622635200,
    };
    
    let cloneable_msg = make_cloneable(another_message);
    
    for i in 1..=3 {
        if let Some(cloned) = cloneable_msg.try_clone() {
            if let Some(value) = cloned.downcast_ref::<CustomMessage>() {
                println!("克隆 #{}: ID={}, 负载={}", 
                         i, value.id, value.payload);
            }
        }
    }
    
    // 演示在周期性消息中的应用
    println!("\n在周期性消息调度中的应用示例:");
    println!("在实际系统中，可以这样使用消息克隆功能:");
    println!("```");
    println!("let tick_message = make_cloneable(MyTickMessage {{ count: 0 }});");
    println!("ctx.schedule_periodic(");
    println!("    ctx.get_self_ref(),");
    println!("    tick_message,");
    println!("    Duration::from_secs(1),");
    println!("    Duration::from_secs(5)");
    println!(").await?;");
    println!("```");
    println!("系统将自动为每次调度创建消息的新副本。");
    
    // 演示错误处理
    println!("\n错误处理:");
    
    // 尝试使用clone_or_panic处理非可克隆类型
    let another_non_cloneable: BoxedMessage = Box::new(NonCloneable(123));
    
    println!("尝试对不可克隆类型调用clone_or_panic将导致panic");
    println!("例如: non_cloneable.clone_or_panic()");
    
    // 安全地使用try_clone()
    match another_non_cloneable.try_clone() {
        Some(_) => println!("消息可以克隆"),
        None => println!("消息不可克隆 - 需要使用默认值或其他策略")
    }
} 