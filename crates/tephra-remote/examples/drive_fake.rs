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
//! With a third argument, `edits`, it serves a small tree for
//! `scripts/verify/pr-11.sh` instead (the folder `Notes` with `Plan.md`, id
//! `plan`, and `Other.md`, id `other`) and also takes these commands (an
//! answer is `ok <value>`; a newline in a text is written `\n`):
//!
//!   text ID            the bytes Drive holds for a file
//!   head ID            its head revision id
//!   uploads ID         how many uploads Drive has applied to it
//!   edit ID TEXT       somebody else changes the file (a new revision)
//!   fail-uploads N S   answer the next N uploads with status S
//!   hold-upload WHEN   hold uploads `before` the bytes are applied or `after`
//!   release-uploads    let the held uploads go
//!   held               how many uploads wait at the hold
//!   writes             how many requests that were not a GET were seen
//!
//! With a third argument `pins` it serves a tree for `scripts/verify/pr-12-pins.sh`
//! instead: the My Drive folder `Notes` holds the folder `Trip Plans` (40 Markdown
//! files `t00.md` .. `t39.md` of about 8 KB each, `t01.md` linking `[[t02]]`, ids `t00` .. `t39`, and the
//! Google Sheet `Budget`), the folder `Elsewhere` (`e00.md` .. `e04.md`) and
//! `top.md`. `add ID NAME TEXT` then puts the new file in `Trip Plans` (not in
//! `Notes`), and the commands above work as in the default tree.
//!
//! The default tree: 395 filler notes `n001.md` .. `n395.md`, `Roadmap.md`, the
//! Google Doc `Meeting` (which links `[[Roadmap]]`), two files called
//! `Plan.md` (ids `plan1`, older, and `plan2`), an empty `brief.pdf`, the
//! Google Sheet `Budget` and `logo.png`. That is 400 files the index reads
//! and two listed by name only.
//! It binds loopback only, and exits when stdin closes.

use std::io::BufRead;

use tephra_remote::testing::{FakeDrive, UploadHold};

const MD: &str = "text/markdown";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(url_file), Some(log_file)) = (args.next(), args.next()) else {
        eprintln!("usage: drive_fake URL_FILE REQUEST_LOG [edits]");
        std::process::exit(2);
    };
    let mode = args.next();
    let edits = mode.as_deref() == Some("edits");
    let pins = mode.as_deref() == Some("pins");

    let drive = FakeDrive::start().await;
    if pins {
        pins_tree(&drive);
    } else if edits {
        drive.add_folder("notes", "Notes", None);
        drive.add_file(
            "plan",
            "Plan.md",
            MD,
            Some("notes"),
            b"# Plan\n\nfirst version\n",
        );
        drive.add_file("other", "Other.md", MD, Some("notes"), b"# Other\n");
    } else {
        full_tree(&drive);
    }
    std::fs::write(&url_file, drive.base_url()).expect("write the URL file");
    serve(drive, log_file, if pins { "trip" } else { "notes" }).await;
}

/// About 8 KB of text for file `n` of `Trip Plans`: distinct per file, so a
/// wrong file would show.
fn trip_text(n: usize) -> Vec<u8> {
    let mut text = format!("# Trip {n:02}\n\n");
    if n == 1 {
        text.push_str("See [[t02]].\n");
    }
    while text.len() < 8000 {
        text.push_str(&format!("line of trip file {n:02}\n"));
    }
    text.into_bytes()
}

fn pins_tree(drive: &FakeDrive) {
    drive.add_folder("notes", "Notes", None);
    drive.add_folder("trip", "Trip Plans", Some("notes"));
    drive.add_folder("else", "Elsewhere", Some("notes"));
    for n in 0..40 {
        drive.add_file(
            &format!("t{n:02}"),
            &format!("t{n:02}.md"),
            MD,
            Some("trip"),
            &trip_text(n),
        );
    }
    drive.add_native(
        "budget",
        "Budget",
        "application/vnd.google-apps.spreadsheet",
        Some("trip"),
    );
    for n in 0..5 {
        drive.add_file(
            &format!("e{n:02}"),
            &format!("e{n:02}.md"),
            MD,
            Some("else"),
            format!("# Elsewhere {n}\n").as_bytes(),
        );
    }
    drive.add_file("top", "top.md", MD, Some("notes"), b"# top\n");
}

fn full_tree(drive: &FakeDrive) {
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
}

async fn serve(drive: FakeDrive, log_file: String, add_parent: &str) {
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
                drive.add_file(a, &format!("{a}.md"), MD, Some(add_parent), text.as_bytes());
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
            "text" if !a.is_empty() => format!(
                "ok {}",
                String::from_utf8_lossy(&drive.content(a)).replace('\n', "\\n")
            ),
            "head" if !a.is_empty() => format!("ok {}", drive.head_revision(a)),
            "uploads" if !a.is_empty() => format!("ok {}", drive.upload_count(a)),
            "edit" if !a.is_empty() => {
                let text = format!("{b} {c}").trim().replace("\\n", "\n");
                drive.update_file(a, format!("{text}\n").as_bytes());
                format!("ok edit {a}")
            }
            "fail-uploads" => match (a.trim().parse::<u32>(), b.trim().parse::<u16>()) {
                (Ok(n), Ok(status)) => {
                    drive.fail_uploads(n, status);
                    format!("ok fail-uploads {n}")
                }
                _ => "error: fail-uploads N STATUS".to_string(),
            },
            "hold-upload" => match a.trim() {
                "before" => {
                    drive.hold_uploads(UploadHold::BeforeApply);
                    "ok hold-upload before".to_string()
                }
                "after" => {
                    drive.hold_uploads(UploadHold::AfterApply);
                    "ok hold-upload after".to_string()
                }
                _ => "error: hold-upload before|after".to_string(),
            },
            "release-uploads" => {
                drive.release_uploads();
                "ok release-uploads".to_string()
            }
            "held" => format!("ok {}", drive.uploads_held()),
            "writes" => format!(
                "ok {}",
                drive
                    .request_log()
                    .all()
                    .iter()
                    .filter(|r| r.method != "GET")
                    .count()
            ),
            "quit" => {
                println!("ok quit");
                return;
            }
            other => format!("error: unknown command `{other}`"),
        };
        println!("{answer}");
    }
}
