//! A fake Google Drive you steer from a script, for `scripts/verify/pr-10.sh`.
//!
//!   cargo run -p tephra-remote --features test-support --example drive_fake -- URL_FILE REQUEST_LOG
//!
//! It serves a My Drive folder `Notes` of 402 files on a loopback port the
//! system picks, writes the base URL to URL_FILE, then reads one command per
//! line on stdin, answering each with one line on stdout:
//!
//!   hold N             answer N more content reads, then hold the rest
//!   release            let the held reads (and every later one) through
//!   add ID NAME TEXT   a new Markdown file in Notes with that text
//!   rename ID NAME     rename a file
//!   remove ID          delete a file
//!   clear              forget the requests so far
//!   dump               write the requests so far to REQUEST_LOG, one
//!                      `METHOD PATH QUERY` line each (never a header, so
//!                      never a token)
//!   quit               stop
//!
//! The tree: 395 filler notes `n001.md` .. `n395.md`, `Roadmap.md`, the
//! Google Doc `Meeting` (which links `[[Roadmap]]`), two files called
//! `Plan.md` (ids `plan1`, older, and `plan2`), an empty `brief.pdf`, the
//! Google Sheet `Budget` and `logo.png`. That is 400 files the index reads
//! and two listed by name only.
//! It binds loopback only, and exits when stdin closes.

use std::io::BufRead;

use tephra_remote::testing::FakeDrive;

const MD: &str = "text/markdown";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(url_file), Some(log_file)) = (args.next(), args.next()) else {
        eprintln!("usage: drive_fake URL_FILE REQUEST_LOG");
        std::process::exit(2);
    };

    let drive = FakeDrive::start().await;
    drive.add_folder("notes", "Notes", None);
    for n in 1..=395 {
        let text = if n == 1 {
            "# n001\n\nSee [[Roadmap]].\n".to_string()
        } else {
            format!("# n{n:03}\n")
        };
        drive.add_file(
            &format!("n{n:03}"),
            &format!("n{n:03}.md"),
            MD,
            Some("notes"),
            text.as_bytes(),
        );
    }
    drive.add_file("roadmap", "Roadmap.md", MD, Some("notes"), b"# Roadmap\n");
    drive.add_doc(
        "meeting",
        "Meeting",
        Some("notes"),
        "# Meeting\n\nAgenda in [[Roadmap]].\n",
    );
    drive.add_file("plan1", "Plan.md", MD, Some("notes"), b"# Plan one\n");
    drive.set_created_time("plan1", "2026-01-01T09:00:00.000Z");
    drive.add_file("plan2", "Plan.md", MD, Some("notes"), b"# Plan two\n");
    drive.set_created_time("plan2", "2026-02-01T09:00:00.000Z");
    drive.add_file("brief", "brief.pdf", "application/pdf", Some("notes"), b"");
    drive.add_native(
        "budget",
        "Budget",
        "application/vnd.google-apps.spreadsheet",
        Some("notes"),
    );
    drive.add_file(
        "logo",
        "logo.png",
        "image/png",
        Some("notes"),
        &[0x89, b'P', b'N', b'G'],
    );
    std::fs::write(&url_file, drive.base_url()).expect("write the URL file");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    while let Some(line) = rx.recv().await {
        let mut words = line.splitn(4, ' ');
        let command = words.next().unwrap_or("");
        let (a, b, c) = (
            words.next().unwrap_or(""),
            words.next().unwrap_or(""),
            words.next().unwrap_or(""),
        );
        let answer = match command {
            "hold" => match a.trim().parse::<usize>() {
                Ok(n) => {
                    drive.hold_content_after(n);
                    format!("ok hold {n}")
                }
                Err(_) => "error: hold N".to_string(),
            },
            "release" => {
                drive.release_content();
                "ok release".to_string()
            }
            "add" if !a.is_empty() && !b.is_empty() => {
                let text = format!("{b} {c}");
                let text = format!("# {}\n", text.trim());
                drive.add_file(a, &format!("{a}.md"), MD, Some("notes"), text.as_bytes());
                format!("ok add {a}")
            }
            "rename" if !a.is_empty() && !b.is_empty() => {
                drive.rename_file(a, b);
                format!("ok rename {a}")
            }
            "remove" if !a.is_empty() => {
                drive.remove_file(a);
                format!("ok remove {a}")
            }
            "clear" => {
                drive.request_log().clear();
                "ok clear".to_string()
            }
            "dump" => {
                let mut text = String::new();
                for r in drive.request_log().all() {
                    text.push_str(&format!(
                        "{} {} {}\n",
                        r.method,
                        r.path,
                        r.query.as_deref().unwrap_or("")
                    ));
                }
                std::fs::write(&log_file, text).expect("write the request log");
                "ok dump".to_string()
            }
            "quit" => {
                println!("ok quit");
                return;
            }
            other => format!("error: unknown command `{other}`"),
        };
        println!("{answer}");
    }
}
