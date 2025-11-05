#![cfg(feature = "remote")]
//! Child processes isolate deliberately hostile inputs that used to overflow the
//! stack or run effectively forever. A ready signal excludes executable startup
//! time from the decoding deadline.
use parrot_api::message::serde_remote_deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn attack_must_be_rejected(case: &str) {
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "decode_attack_child", "--nocapture"])
            .env("PARROT_DECODE_ATTACK", case)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line == "DECODER_READY" {
                let _ = ready_tx.send(());
            }
            output.push_str(&line);
            output.push('\n');
        }
        output
    });
    let ready = ready_rx.recv_timeout(Duration::from_secs(180));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut status = None;
    if ready.is_ok() {
        loop {
            status = child.0.try_wait().unwrap();
            if status.is_some() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if status.is_none() {
        let _ = child.0.kill();
        child.0.wait().unwrap();
    }
    let stdout = reader.join().unwrap();
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        ready.is_ok(),
        "child did not reach decoder: {stdout}\n{stderr}"
    );
    assert!(
        status.is_some(),
        "decoder exceeded its execution deadline for {case}: {stdout}\n{stderr}"
    );
    assert!(
        status.unwrap().success(),
        "decoder did not return a rejection for {case}: {stdout}\n{stderr}"
    );
}

#[test]
fn declared_zero_byte_element_count_is_bounded() {
    attack_must_be_rejected("elements");
}

#[test]
fn recursive_payload_is_rejected_before_stack_exhaustion() {
    attack_must_be_rejected("depth");
}

#[test]
fn impossible_string_allocation_is_rejected_without_panicking() {
    attack_must_be_rejected("string");
}

#[test]
#[ignore = "isolated child, invoked by the parent tests"]
fn decode_attack_child() {
    let Ok(case) = std::env::var("PARROT_DECODE_ATTACK") else {
        return;
    };
    println!("DECODER_READY");
    std::io::stdout().flush().unwrap();
    match case.as_str() {
        "elements" => {
            // bincode standard varint: marker 253 followed by a LE u64 length.
            // Unit elements consume no bytes, so a frame-length cap cannot help.
            let mut wire = vec![253];
            wire.extend_from_slice(&u64::MAX.to_le_bytes());
            assert!(serde_remote_deserialize::<Vec<()>>(&wire).is_err());
        }
        "string" => {
            let mut wire = vec![253];
            wire.extend_from_slice(&u64::MAX.to_le_bytes());
            assert!(serde_remote_deserialize::<String>(&wire).is_err());
        }
        "depth" => {
            #[allow(dead_code)]
            #[derive(serde::Deserialize)]
            struct Chain(Option<Box<Chain>>);
            // Each 1 selects Some(Chain); the final 0 selects None.
            let mut wire = vec![1; 100_000];
            wire.push(0);
            assert!(serde_remote_deserialize::<Chain>(&wire).is_err());
        }
        _ => panic!("unknown child case"),
    }
}

use parrot_api::message::{
    serde_remote_deserialize_with_limits, serde_remote_serialize, RemoteDecodeLimits,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[test]
fn ordinary_nested_schema_preserves_bincode_representation() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Label(String);
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Payload {
        Empty,
        Number(Box<u64>),
        Tuple(Vec<u8>, Option<Label>),
        Fields { map: BTreeMap<String, Vec<bool>> },
    }
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Message {
        items: Vec<Payload>,
        scalars: (i128, u128, f32, f64, char),
        tuple: ((), Option<()>),
    }
    let expected = Message {
        items: vec![
            Payload::Empty,
            Payload::Number(Box::new(u64::MAX)),
            Payload::Tuple(vec![0, 127, 255], Some(Label("汉字".into()))),
            Payload::Fields {
                map: BTreeMap::from([("flags".into(), vec![true, false])]),
            },
        ],
        scalars: (i128::MIN, u128::MAX, 0.5, -1.25, 'π'),
        tuple: ((), Some(())),
    };
    let wire = serde_remote_serialize(&expected).unwrap();
    let (reference, consumed): (Message, _) =
        bincode::serde::decode_from_slice(&wire, bincode::config::standard()).unwrap();
    assert_eq!(reference, expected);
    assert_eq!(consumed, wire.len());
    assert_eq!(
        serde_remote_deserialize::<Message>(&wire).unwrap(),
        expected
    );
}

#[test]
fn value_budget_is_shared_across_sibling_containers_and_resets_per_message() {
    let limits = RemoteDecodeLimits {
        max_values: 150,
        ..Default::default()
    };
    let too_many = serde_remote_serialize(&vec![vec![(); 20]; 20]).unwrap();
    let error =
        serde_remote_deserialize_with_limits::<Vec<Vec<()>>>(&too_many, limits).unwrap_err();
    assert!(error.contains("value limit"), "{error}");
    let small = vec![vec![(); 20]; 5];
    let wire = serde_remote_serialize(&small).unwrap();
    for _ in 0..2 {
        assert_eq!(
            serde_remote_deserialize_with_limits::<Vec<Vec<()>>>(&wire, limits).unwrap(),
            small
        );
    }
    let large_limits = RemoteDecodeLimits {
        max_values: 500,
        ..limits
    };
    assert_eq!(
        serde_remote_deserialize_with_limits::<Vec<Vec<()>>>(&too_many, large_limits)
            .unwrap()
            .len(),
        20
    );
}

#[test]
fn map_keys_and_values_share_the_budget() {
    let expected = BTreeMap::from([(1u8, vec![(); 10]), (2, vec![(); 10]), (3, vec![(); 10])]);
    let wire = serde_remote_serialize(&expected).unwrap();
    let limits = RemoteDecodeLimits {
        max_values: 25,
        ..Default::default()
    };
    let error =
        serde_remote_deserialize_with_limits::<BTreeMap<u8, Vec<()>>>(&wire, limits).unwrap_err();
    assert!(error.contains("value limit"), "{error}");
    assert_eq!(
        serde_remote_deserialize::<BTreeMap<u8, Vec<()>>>(&wire).unwrap(),
        expected
    );
}

#[test]
fn enum_recursion_and_option_newtypes_obey_depth_limit() {
    #[allow(dead_code)]
    #[derive(Deserialize)]
    enum Nest {
        End,
        Next(Box<Nest>),
    }
    #[allow(dead_code)]
    #[derive(Deserialize)]
    struct Chain(Option<Box<Chain>>);
    let mut wire = vec![1; 256];
    wire.push(0);
    let limits = RemoteDecodeLimits {
        max_depth: 8,
        ..Default::default()
    };
    let enum_error = serde_remote_deserialize_with_limits::<Nest>(&wire, limits)
        .err()
        .unwrap();
    let chain_error = serde_remote_deserialize_with_limits::<Chain>(&wire, limits)
        .err()
        .unwrap();
    assert!(enum_error.contains("depth limit"), "{enum_error}");
    assert!(chain_error.contains("depth limit"), "{chain_error}");
    assert!(matches!(
        serde_remote_deserialize_with_limits::<Nest>(&[0], limits).unwrap(),
        Nest::End
    ));
}

#[derive(Debug, PartialEq)]
struct StringOnly(String);
impl<'de> Deserialize<'de> for StringOnly {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OwnedVisitor;
        // Deliberately incomplete visitor: this regression fixture detects a
        // decoder silently replacing its established owned callback with a borrow.
        #[allow(
            clippy::serde_api_misuse,
            reason = "fixture deliberately accepts only the owned callback"
        )]
        impl serde::de::Visitor<'_> for OwnedVisitor {
            type Value = StringOnly;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an owned String")
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StringOnly(v))
            }
        }
        deserializer.deserialize_string(OwnedVisitor)
    }
}
#[derive(Debug, PartialEq)]
struct BytesOnly(Vec<u8>);
impl<'de> Deserialize<'de> for BytesOnly {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OwnedVisitor;
        impl serde::de::Visitor<'_> for OwnedVisitor {
            type Value = BytesOnly;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an owned byte buffer")
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                Ok(BytesOnly(v))
            }
        }
        deserializer.deserialize_byte_buf(OwnedVisitor)
    }
}

#[test]
fn owned_visitors_receive_owned_values_after_input_validation() {
    assert_eq!(
        serde_remote_deserialize::<StringOnly>(&[2, b'o', b'k']).unwrap(),
        StringOnly("ok".into())
    );
    assert_eq!(
        serde_remote_deserialize::<BytesOnly>(&[3, 0, 128, 255]).unwrap(),
        BytesOnly(vec![0, 128, 255])
    );
    let mut impossible = vec![253];
    impossible.extend_from_slice(&u64::MAX.to_le_bytes());
    assert!(serde_remote_deserialize::<StringOnly>(&impossible).is_err());
    assert!(serde_remote_deserialize::<BytesOnly>(&impossible).is_err());
    assert!(serde_remote_deserialize::<StringOnly>(&[1, 255]).is_err());
    assert!(serde_remote_deserialize::<BytesOnly>(&[3, 1]).is_err());
}

#[test]
fn payload_cap_is_checked_before_invoking_application_deserializer() {
    struct MustNotRun;
    impl<'de> Deserialize<'de> for MustNotRun {
        fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
            panic!("oversized bytes reached application deserializer")
        }
    }
    let limits = RemoteDecodeLimits {
        max_payload_bytes: 3,
        ..Default::default()
    };
    let error = serde_remote_deserialize_with_limits::<MustNotRun>(&[0; 4], limits)
        .err()
        .unwrap();
    assert!(error.contains("payload byte limit"));
    assert_eq!(
        serde_remote_deserialize_with_limits::<String>(&[2, b'o', b'k'], limits).unwrap(),
        "ok"
    );
    assert!(serde_remote_deserialize_with_limits::<u8>(
        &[0],
        RemoteDecodeLimits {
            max_values: 0,
            ..limits
        }
    )
    .is_err());
    assert!(serde_remote_deserialize_with_limits::<u8>(
        &[0],
        RemoteDecodeLimits {
            max_depth: 0,
            ..limits
        }
    )
    .is_err());
    assert!(serde_remote_deserialize_with_limits::<u8>(
        &[0],
        RemoteDecodeLimits {
            max_depth: 129,
            ..limits
        }
    )
    .is_err());
}

#[test]
fn byte_buffers_do_not_consume_one_value_per_byte_and_suffixes_still_fail() {
    let bytes = vec![42u8; 4096];
    let wire = serde_remote_serialize(&bytes).unwrap();
    let limits = RemoteDecodeLimits {
        max_values: 1,
        ..Default::default()
    };
    assert_eq!(
        serde_remote_deserialize_with_limits::<BytesOnly>(&wire, limits)
            .unwrap()
            .0,
        bytes
    );
    // A generic sequence is different: its elements are individually visited.
    assert!(serde_remote_deserialize_with_limits::<Vec<u8>>(&wire, limits).is_err());
    assert!(serde_remote_deserialize::<()>(&[0])
        .err()
        .unwrap()
        .contains("trailing bytes"));
}

// Measure only this test thread's decode window. This catches allocation-before-
// EOF validation even when the declared length is small enough not to panic.
struct MeasuringAllocator;
thread_local! {
    static MEASURE_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
static LARGEST_ALLOCATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn record_allocation(size: usize) {
    if MEASURE_ALLOCATION
        .try_with(|enabled| enabled.get())
        .unwrap_or(false)
    {
        LARGEST_ALLOCATION.fetch_max(size, std::sync::atomic::Ordering::Relaxed);
    }
}
// SAFETY: Every operation delegates to System with the original allocation's
// layout and pointer. Accounting never allocates or dereferences those pointers.
unsafe impl std::alloc::GlobalAlloc for MeasuringAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: GlobalAlloc's caller guarantees a valid layout.
        unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: GlobalAlloc's caller guarantees a valid layout.
        unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: This pointer was allocated by System using the same layout.
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        record_allocation(size);
        // SAFETY: The original allocation and the requested size obey GlobalAlloc's contract.
        unsafe { std::alloc::GlobalAlloc::realloc(&std::alloc::System, ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: MeasuringAllocator = MeasuringAllocator;

#[test]
fn missing_buffer_data_does_not_allocate_its_claimed_megabytes() {
    struct ResetMeasurement;
    impl Drop for ResetMeasurement {
        fn drop(&mut self) {
            MEASURE_ALLOCATION.set(false);
        }
    }
    let wire = serde_remote_serialize(&(8 * 1024 * 1024u64)).unwrap();
    assert!(wire.len() <= 9);
    LARGEST_ALLOCATION.store(0, std::sync::atomic::Ordering::Relaxed);
    MEASURE_ALLOCATION.set(true);
    let guard = ResetMeasurement;
    let string = serde_remote_deserialize::<String>(&wire);
    let bytes = serde_remote_deserialize::<BytesOnly>(&wire);
    drop(guard);
    assert!(string.is_err());
    assert!(bytes.is_err());
    let largest = LARGEST_ALLOCATION.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        largest < 16 * 1024,
        "a tiny truncated payload requested a {largest}-byte allocation"
    );
}

#[test]
fn custom_zero_byte_deserializers_still_consume_the_global_budget() {
    #[derive(Clone, Debug, Serialize)]
    struct Zero;
    impl<'de> Deserialize<'de> for Zero {
        fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
            Ok(Zero)
        }
    }
    let wire = serde_remote_serialize(&vec![vec![Zero; 20]; 20]).unwrap();
    let limits = RemoteDecodeLimits {
        max_values: 150,
        ..Default::default()
    };
    let error = serde_remote_deserialize_with_limits::<Vec<Vec<Zero>>>(&wire, limits).unwrap_err();
    assert!(error.contains("value limit"), "{error}");
    let root_error = serde_remote_deserialize_with_limits::<Zero>(
        &[],
        RemoteDecodeLimits {
            max_values: 0,
            ..limits
        },
    )
    .unwrap_err();
    assert!(root_error.contains("value limit"));
}
