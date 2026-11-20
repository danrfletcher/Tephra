//! Throwaway S3 spike: watch a directory the way `tephrad` does (the
//! `notify` crate's recommended watcher, recursive) and log every raw
//! event, including the ones under `.git`, without any ignore filtering.
//!
//!   spike-watcher <dir> <events-file> <ready-file> <stop-file>
//!
//! Each event is one line: `<epoch ms>\t<kind>\t<path relative to dir>`.
//! The ready file is created once the watch is registered; the program
//! exits when the stop file appears.

use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: spike-watcher <dir> <events-file> <ready-file> <stop-file>");
        std::process::exit(2);
    }
    let root = fs::canonicalize(&args[1]).expect("canonicalize watch dir");
    let events_path = PathBuf::from(&args[2]);
    let ready = PathBuf::from(&args[3]);
    let stop = PathBuf::from(&args[4]);

    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&events_path)
        .expect("open events file");

    let (tx, rx) = mpsc::channel();
    let mut watcher = recommended_watcher(tx).expect("create watcher");
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .expect("watch dir");
    fs::write(&ready, b"ready\n").expect("write ready file");

    loop {
        if stop.exists() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                let kind = format!("{:?}", event.kind).replace(' ', "");
                for path in &event.paths {
                    let rel = path.strip_prefix(&root).unwrap_or(path);
                    let _ = writeln!(out, "{}\t{}\t{}", now_ms(), kind, rel.display());
                }
            }
            Ok(Err(err)) => {
                let _ = writeln!(out, "{}\tERROR\t{}", now_ms(), err);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
