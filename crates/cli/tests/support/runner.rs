//! The test runner of the `harness = false` binaries (`daemon_start`, `daemon_stop`,
//! `daemon_open`, `daemon_macos`, `gates`): each binary is also a `chaldea` with test seams
//! ([`super::faults`]), so the child `daemon start` launches (`current_exe()`) is the same
//! binary, and `gates`'s waiter is too. libtest's protocol as far
//! as `cargo test` and the helpers use it: `--list`, name filters, `--exact`,
//! `--test-threads=1`; cases run in parallel otherwise, each on its own thread.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

/// Set: the binary blocks until a line (or EOF) arrives on stdin, then exits 0: a live process
/// unrelated to any daemon, for tests that must see it unsignalled.
pub const SLEEP_ON_STDIN: &str = "CHALDEA_TEST_SLEEP_ON_STDIN";

/// One test case.
pub struct Case {
    pub name: &'static str,
    pub run: fn(),
}

/// The binary's `main`: acts as `chaldea` when the fault entry's variable is set, else runs
/// the selected cases and exits 101 when one failed.
pub fn main(cases: &[Case]) {
    super::faults::enter_if_chaldea();
    if std::env::var_os(SLEEP_ON_STDIN).is_some() {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        return;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for c in cases {
            println!("{}: test", c.name);
        }
        return;
    }
    let exact = args.iter().any(|a| a == "--exact");
    let serial = args.iter().any(|a| a == "--test-threads=1");
    let filters: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect();
    let selected: Vec<&Case> = cases
        .iter()
        .filter(|c| {
            filters.is_empty()
                || filters.iter().any(|f| {
                    if exact {
                        c.name == *f
                    } else {
                        c.name.contains(f)
                    }
                })
        })
        .collect();
    println!("\nrunning {} tests", selected.len());
    let run_one = |c: &Case| -> (Result<(), ()>, Duration) {
        let t = Instant::now();
        let r = catch_unwind(AssertUnwindSafe(c.run)).map_err(|_| ());
        (r, t.elapsed())
    };
    let results: Vec<(&str, Result<(), ()>, Duration)> = if serial {
        selected
            .iter()
            .map(|c| {
                let (r, d) = run_one(c);
                (c.name, r, d)
            })
            .collect()
    } else {
        std::thread::scope(|s| {
            let handles: Vec<_> = selected
                .iter()
                .map(|c| (c.name, s.spawn(move || run_one(c))))
                .collect();
            handles
                .into_iter()
                .map(|(n, h)| {
                    let (r, d) = h.join().unwrap_or((Err(()), Duration::ZERO));
                    (n, r, d)
                })
                .collect()
        })
    };
    let mut failed = Vec::new();
    for (name, r, d) in &results {
        match r {
            Ok(()) => println!("test {name} ... ok ({:.1} s)", d.as_secs_f64()),
            Err(()) => {
                println!("test {name} ... FAILED ({:.1} s)", d.as_secs_f64());
                failed.push(*name);
            }
        }
    }
    if failed.is_empty() {
        println!("\ntest result: ok. {} passed; 0 failed\n", results.len());
    } else {
        println!(
            "\nfailures: {}\ntest result: FAILED. {} passed; {} failed\n",
            failed.join(", "),
            results.len() - failed.len(),
            failed.len()
        );
        std::process::exit(101);
    }
}
