//! Untimed reference for the Erlang implementation. This uses the independently
//! reviewed Rust recurrence and checks literal vectors before generating data.
#[allow(dead_code)]
#[path = "../actix-bench/src/oracle.rs"]
mod reference;
use std::{env, fs, io::Write, thread};
fn main() {
    let args: Vec<_> = env::args().collect();
    assert_eq!(args.len(), 3, "usage: oracle INPUT OUTPUT");
    for (iterations, salt, expected) in [
        (0, 0, 1),
        (1, 0, 7_806_831_264_735_756_412),
        (1, 1, 7_806_831_264_735_756_412),
        (2, 0, 9_396_908_728_118_811_418),
        (10, 7, 6_404_011_227_278_016_048),
        (64, 15, 2_931_105_770_476_240_399),
        (2, 3_639_755_004_476_521_255, 10_481_999_410_520_546_992),
        (1, u64::MAX, 13_525_302_890_751_722_018),
    ] {
        assert_eq!(reference::reference(iterations, salt), expected);
    }
    let mut keys: Vec<(u64, u64)> = fs::read_to_string(&args[1])
        .unwrap()
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 2);
            (fields[0].parse().unwrap(), fields[1].parse().unwrap())
        })
        .collect();
    keys.sort_unstable();
    let original = keys.len();
    keys.dedup();
    assert_eq!(original, keys.len());
    keys.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
    let count = thread::available_parallelism().unwrap().get().min(16);
    let mut groups = vec![Vec::new(); count];
    for (i, key) in keys.into_iter().enumerate() {
        groups[i % count].push(key);
    }
    let workers: Vec<_> = groups
        .into_iter()
        .map(|group| {
            thread::spawn(move || {
                group
                    .into_iter()
                    .map(|(n, s)| (n, s, reference::reference(n, s)))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut results = Vec::new();
    for worker in workers {
        results.extend(worker.join().expect("oracle worker failed"));
    }
    results.sort_unstable();
    let mut output = fs::File::create(&args[2]).unwrap();
    for (n, s, v) in results {
        writeln!(output, "{{{{{n},{s}}},{v}}}.").unwrap();
    }
    output.sync_all().unwrap();
    eprintln!("oracle complete: {original} independent results; {count} joined workers");
}
