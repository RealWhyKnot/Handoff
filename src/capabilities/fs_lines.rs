pub(crate) const MAX_LINES: u64 = 5000;
pub(super) const REPLY_CAP: usize = 1 << 20;
pub(super) const LINE_SHOW: usize = 8 * 1024;
pub(super) const CHUNK: usize = 64 * 1024;
pub(super) const CANCELLED: &str = "stopped early because the command was cancelled";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::fs_lines_grep::grep;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use crate::capabilities::fs_lines_range::range;
    use crate::capabilities::fs_lines_tail::tail;

    use crate::dispatch::Args;
    use crate::dispatch::{Ctx, Outcome, Router};
    use serde_json::json;
    use std::fs;

    use std::io::{BufWriter, Write};
    use std::path::Path;

    fn args(v: Value) -> Args {
        serde_json::from_value(v).unwrap_or_default()
    }

    async fn run(kind: &str, v: Value) -> Outcome {
        let mut r = Router::new();
        super::super::fs::register(&mut r);
        r.dispatch(Ctx::default(), kind, args(v)).await
    }

    fn lines(o: &Outcome) -> Vec<String> {
        o.result["lines"]
            .as_array()
            .unwrap_or_else(|| panic!("no lines: {} {}", o.error, o.result))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn utf16(text: &str, le: bool) -> Vec<u8> {
        let mut out = if le {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for u in text.encode_utf16() {
            out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        out
    }

    fn big_line(i: u64) -> String {
        format!("line {i:08} the quick brown fox jumps over the lazy dog 0123456789")
    }

    #[tokio::test]
    async fn a_50_mb_file_reads_in_the_middle_and_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let total = 800_000u64;
        {
            let mut w = BufWriter::new(fs::File::create(&path).unwrap());
            for i in 1..=total {
                writeln!(w, "{}", big_line(i)).unwrap();
            }
        }
        assert!(fs::metadata(&path).unwrap().len() >= 50 * 1024 * 1024);
        let path = path.to_string_lossy().into_owned();

        let mid = run(
            "fs.read",
            json!({"path": path, "start_line": 400_000, "end_line": 400_004}),
        )
        .await;
        assert!(mid.ok, "{}", mid.error);
        let got = lines(&mid);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0], big_line(400_000));
        assert_eq!(got[4], big_line(400_004));
        assert_eq!(mid.result["first_line"], json!(400_000));
        assert_eq!(mid.result["last_line"], json!(400_004));
        assert_eq!(mid.result["eof"], json!(false));
        assert_eq!(mid.result["truncated"], json!(false));
        assert!(mid.result.get("base64").is_none());

        let end = run(
            "fs.read",
            json!({"path": path, "start_line": total - 2, "end_line": total + 100}),
        )
        .await;
        assert!(end.ok, "{}", end.error);
        assert_eq!(
            lines(&end),
            vec![big_line(total - 2), big_line(total - 1), big_line(total)]
        );
        assert_eq!(end.result["eof"], json!(true));
        assert_eq!(end.result["total_lines"], json!(total));
        assert_eq!(end.result["truncated"], json!(false));

        let tail = run("fs.tail", json!({"path": path, "lines": 3})).await;
        assert!(tail.ok, "{}", tail.error);
        assert_eq!(
            lines(&tail),
            vec![big_line(total - 2), big_line(total - 1), big_line(total)]
        );

        let head = run("fs.head", json!({"path": path, "lines": 2})).await;
        assert_eq!(lines(&head), vec![big_line(1), big_line(2)]);
        assert_eq!(head.result["eof"], json!(false));

        let found = run(
            "fs.grep",
            json!({"path": path, "text": "LINE 00799999 ", "before": 1}),
        )
        .await;
        assert!(found.ok, "{}", found.error);
        assert_eq!(found.result["match_count"], json!(1));
        assert_eq!(found.result["match_lines"], json!([799_999]));
        assert_eq!(found.result["lines_scanned"], json!(total));
        assert_eq!(
            lines(&found),
            vec![
                format!("799998- {}", big_line(799_998)),
                format!("799999: {}", big_line(799_999)),
            ]
        );

        let span = run(
            "fs.read",
            json!({"path": path, "start_line": 10, "end_line": 20_000}),
        )
        .await;
        assert!(span.ok, "{}", span.error);
        assert_eq!(span.result["line_count"], json!(5000));
        assert_eq!(span.result["last_line"], json!(5009));
        assert_eq!(span.result["truncated"], json!(true));
        assert_eq!(
            span.result["note"],
            json!("only 5000 lines fit in one reply; read on from line 5010")
        );

        let whole = run("fs.read", json!({"path": path})).await;
        assert!(!whole.ok);
        for want in [
            "over the 8 MiB limit",
            "start_line and end_line",
            "fs.head",
            "fs.tail",
            "fs.grep",
            "or download it.",
        ] {
            assert!(whole.error.contains(want), "{want}: {}", whole.error);
        }
        let dl = super::super::fs_read::fs_download(&args(json!({"path": path}))).unwrap_err();
        assert!(
            dl.message
                .ends_with("Download it in parts with offset and length."),
            "{}",
            dl.message
        );
    }

    #[tokio::test]
    async fn utf16_files_with_a_bom_decode_and_number_lines() {
        let dir = tempfile::tempdir().unwrap();
        let text = "first\r\nsecond \u{fc}ber\r\nthird\r\n";
        for (name, le, enc) in [("le.txt", true, "utf-16le"), ("be.txt", false, "utf-16be")] {
            let path = write(dir.path(), name, &utf16(text, le));
            let head = run("fs.head", json!({"path": path})).await;
            assert!(head.ok, "{}", head.error);
            assert_eq!(lines(&head), vec!["first", "second \u{fc}ber", "third"]);
            assert_eq!(head.result["encoding"], json!(enc));
            assert_eq!(head.result["total_lines"], json!(3));

            let mid = run(
                "fs.read",
                json!({"path": path, "start_line": 2, "end_line": 2}),
            )
            .await;
            assert_eq!(lines(&mid), vec!["second \u{fc}ber"]);

            let tail = run("fs.tail", json!({"path": path, "lines": 2})).await;
            assert_eq!(lines(&tail), vec!["second \u{fc}ber", "third"]);

            let hit = run("fs.grep", json!({"path": path, "text": "\u{dc}BER"})).await;
            assert!(hit.ok, "{}", hit.error);
            assert_eq!(hit.result["match_lines"], json!([2]));
            assert_eq!(lines(&hit), vec!["2: second \u{fc}ber"]);
        }
    }

    #[tokio::test]
    async fn utf8_bom_and_bad_bytes_decode_lossily() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "bom.txt",
            b"\xEF\xBB\xBFone\nbad \xFF byte\nthree",
        );
        let out = run("fs.head", json!({"path": path})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(lines(&out), vec!["one", "bad \u{FFFD} byte", "three"]);
        assert_eq!(out.result["encoding"], json!("utf-8 bom"));
        let tail = run("fs.tail", json!({"path": path, "lines": 5})).await;
        assert_eq!(lines(&tail), vec!["one", "bad \u{FFFD} byte", "three"]);
    }

    #[tokio::test]
    async fn crlf_and_lf_files_number_lines_the_same() {
        let dir = tempfile::tempdir().unwrap();
        for (name, nl) in [("crlf.txt", "\r\n"), ("lf.txt", "\n")] {
            let body = ["a", "b", "", "needle", "c"].join(nl) + nl;
            let path = write(dir.path(), name, body.as_bytes());
            let hit = run(
                "fs.grep",
                json!({"path": path, "text": "needle", "after": 1}),
            )
            .await;
            assert!(hit.ok, "{}", hit.error);
            assert_eq!(hit.result["match_lines"], json!([4]));
            assert_eq!(lines(&hit), vec!["4: needle", "5- c"]);
            let one = run(
                "fs.read",
                json!({"path": path, "start_line": 3, "end_line": 5}),
            )
            .await;
            assert_eq!(lines(&one), vec!["", "needle", "c"]);
            assert_eq!(one.result["total_lines"], json!(5));
            let tail = run("fs.tail", json!({"path": path, "lines": 2})).await;
            assert_eq!(lines(&tail), vec!["needle", "c"]);
        }
    }

    #[tokio::test]
    async fn ranges_past_the_end_and_backwards_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "three.txt", b"x\ny\nz\n");
        let past = run(
            "fs.read",
            json!({"path": path, "start_line": 10, "end_line": 20}),
        )
        .await;
        assert!(past.ok, "{}", past.error);
        assert_eq!(past.result["line_count"], json!(0));
        assert_eq!(past.result["first_line"], Value::Null);
        assert_eq!(past.result["eof"], json!(true));
        assert_eq!(past.result["note"], json!("the file has only 3 lines"));

        let only_end = run("fs.read", json!({"path": path, "end_line": 2})).await;
        assert_eq!(lines(&only_end), vec!["x", "y"]);
        assert_eq!(only_end.result["eof"], json!(false));

        let back = run(
            "fs.read",
            json!({"path": path, "start_line": 3, "end_line": 2}),
        )
        .await;
        assert!(!back.ok);
        assert_eq!(back.error, "fs.read: start_line is after end_line");

        let empty = write(dir.path(), "empty.txt", b"");
        for kind in ["fs.head", "fs.tail"] {
            let out = run(kind, json!({"path": empty})).await;
            assert!(out.ok, "{kind}: {}", out.error);
            assert_eq!(out.result["line_count"], json!(0), "{kind}");
        }
        let out = run("fs.head", json!({"path": empty})).await;
        assert_eq!(out.result["note"], json!("the file is empty"));
    }

    #[tokio::test]
    async fn replies_stay_under_the_size_cap_and_long_lines_are_cut() {
        let dir = tempfile::tempdir().unwrap();
        let row = "w".repeat(3000);
        let mut body = String::new();
        for _ in 0..1000 {
            body.push_str(&row);
            body.push('\n');
        }
        let path = write(dir.path(), "wide.txt", body.as_bytes());
        let out = run("fs.head", json!({"path": path, "lines": 1000})).await;
        assert!(out.ok, "{}", out.error);
        let count = out.result["line_count"].as_u64().unwrap();
        assert!(count > 300 && count < 1000, "{count}");
        assert_eq!(out.result["truncated"], json!(true));
        let note = out.result["note"].as_str().unwrap();
        assert!(
            note.starts_with(&format!("stopped at line {count} ")),
            "{note}"
        );
        assert!(
            note.ends_with(&format!("read on from line {}", count + 1)),
            "{note}"
        );
        assert!(serde_json::to_vec(&out.result).unwrap().len() < REPLY_CAP + 4096);

        let tail = run("fs.tail", json!({"path": path, "lines": 1000})).await;
        let tcount = tail.result["line_count"].as_u64().unwrap();
        assert!(tcount > 300 && tcount < 1000, "{tcount}");
        assert_eq!(
            tail.result["note"],
            json!(format!("only the last {tcount} lines fit in 1 MiB"))
        );

        let long = format!(
            "start {} NEEDLE {}\nshort\n",
            "a".repeat(20_000),
            "b".repeat(100)
        );
        let path = write(dir.path(), "long.txt", long.as_bytes());
        let head = run("fs.head", json!({"path": path})).await;
        let got = lines(&head);
        assert!(got[0].starts_with("start aaa") && got[0].ends_with("..."));
        assert!(got[0].len() <= LINE_SHOW + 3);
        assert_eq!(got[1], "short");
        assert_eq!(head.result["note"], json!("1 long line was cut to 8 KiB"));

        let hit = run("fs.grep", json!({"path": path, "text": "needle"})).await;
        let got = lines(&hit);
        assert!(
            got[0].starts_with("1: ...") && got[0].contains("NEEDLE bbb"),
            "{}",
            got[0]
        );
        assert_eq!(hit.result["truncated"], json!(true));
    }

    #[tokio::test]
    async fn match_cap_still_counts_every_match() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=50).map(|i| format!("hit {i}\nmiss\n")).collect();
        let path = write(dir.path(), "hits.txt", body.as_bytes());
        let out = run(
            "fs.grep",
            json!({"path": path, "text": "hit", "max_matches": 10}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["listed"], json!(10));
        assert_eq!(out.result["match_count"], json!(50));
        assert_eq!(out.result["truncated"], json!(true));
        assert_eq!(
            out.result["note"],
            json!("listed 10 of 50 matches; search again with start_line 20 for more")
        );
        let next = run(
            "fs.grep",
            json!({"path": path, "text": "hit", "max_matches": 10, "start_line": 20}),
        )
        .await;
        assert_eq!(next.result["match_lines"][0], json!(21));
        assert_eq!(next.result["match_count"], json!(40));
    }

    #[tokio::test]
    async fn overlapping_context_is_merged() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=20)
            .map(|i| {
                if i == 5 || i == 7 || i == 15 {
                    format!("hit {i}\n")
                } else {
                    format!("x{i}\n")
                }
            })
            .collect();
        let path = write(dir.path(), "ctx.txt", body.as_bytes());
        let out = run(
            "fs.grep",
            json!({"path": path, "text": "hit", "before": 2, "after": 2}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(
            lines(&out),
            vec![
                "3- x3",
                "4- x4",
                "5: hit 5",
                "6- x6",
                "7: hit 7",
                "8- x8",
                "9- x9",
                "--",
                "13- x13",
                "14- x14",
                "15: hit 15",
                "16- x16",
                "17- x17",
            ]
        );
        assert_eq!(out.result["match_lines"], json!([5, 7, 15]));

        let plain = run("fs.grep", json!({"path": path, "text": "hit"})).await;
        assert_eq!(lines(&plain), vec!["5: hit 5", "7: hit 7", "15: hit 15"]);
    }

    #[tokio::test]
    async fn case_matching_can_be_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "case.txt",
            b"Error here\nerror there\nERROR\nfine\n",
        );
        let loose = run("fs.grep", json!({"path": path, "text": "error"})).await;
        assert_eq!(loose.result["match_lines"], json!([1, 2, 3]));
        assert_eq!(loose.result["case_sensitive"], json!(false));
        let strict = run(
            "fs.grep",
            json!({"path": path, "text": "error", "case_sensitive": true}),
        )
        .await;
        assert_eq!(strict.result["match_lines"], json!([2]));
        assert_eq!(strict.result["match_count"], json!(1));
    }

    #[test]
    fn cancelling_stops_every_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "c.txt", b"one\ntwo\nthree\n");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let g = grep(&args(json!({"path": path, "text": "o"})), &cancel).unwrap();
        assert_eq!(g["lines_scanned"], json!(0));
        assert_eq!(g["truncated"], json!(true));
        assert_eq!(g["note"], json!(CANCELLED));
        let r = range(&args(json!({"path": path, "start_line": 1})), &cancel).unwrap();
        assert_eq!(r["truncated"], json!(true));
        assert_eq!(r["eof"], json!(false));
        assert_eq!(r["note"], json!(CANCELLED));
        let t = tail(&args(json!({"path": path})), &cancel).unwrap();
        assert_eq!(t["line_count"], json!(0));
        assert_eq!(t["note"], json!(CANCELLED));
    }

    #[tokio::test]
    async fn secret_paths_and_missing_files_are_refused_plainly() {
        let secret = "C:\\Users\\x\\.ssh\\id_rsa";
        for (kind, extra) in [
            ("fs.grep", json!({"text": "a"})),
            ("fs.read", json!({"start_line": 1})),
            ("fs.head", json!({})),
            ("fs.tail", json!({})),
        ] {
            let mut a = extra.as_object().cloned().unwrap();
            a.insert("path".into(), json!(secret));
            let out = run(kind, Value::Object(a)).await;
            assert!(!out.ok, "{kind}");
            assert!(
                out.error.contains("can hold passwords or keys"),
                "{kind}: {}",
                out.error
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt").to_string_lossy().into_owned();
        for kind in ["fs.read", "fs.head", "fs.tail", "fs.grep", "fs.stat"] {
            let out = run(kind, json!({"path": missing, "text": "a"})).await;
            assert_eq!(out.error, format!("{kind}: {missing} doesn't exist"));
        }
        let folder = dir.path().to_string_lossy().into_owned();
        for kind in ["fs.read", "fs.head", "fs.grep"] {
            let out = run(kind, json!({"path": folder, "text": "a"})).await;
            assert_eq!(
                out.error,
                format!("{kind}: {folder} is a folder, not a file")
            );
        }
        let file = write(dir.path(), "f.txt", b"x");
        let ls = run("fs.ls", json!({"path": file})).await;
        assert_eq!(ls.error, format!("fs.ls: {file} is a file, not a folder"));
    }

    #[tokio::test]
    async fn grep_needs_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "t.txt", b"x\n");
        let out = run("fs.grep", json!({"path": path, "text": ""})).await;
        assert!(!out.ok);
        assert!(out.error.contains("'text'"), "{}", out.error);
    }
}
