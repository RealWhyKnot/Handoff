use crate::console::{char_width, display_width, plural};
use crate::dispatch::hidden_char;

const TAB_STOP: usize = 4;
const BREAK_AFTER: [char; 8] = [' ', '\\', '/', '|', ',', ';', '&', '?'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum More {
    Lines(usize),
    Chars(usize),
}

impl More {
    pub fn note(&self) -> String {
        match self {
            More::Lines(n) => format!("... {}", plural(*n, "more line", "more lines")),
            More::Chars(n) => format!("... {}", plural(*n, "more character", "more characters")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folded {
    pub rows: Vec<String>,
    pub more: Option<More>,
}

pub fn clean_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
                col = 0;
            }
            '\n' | '\u{2028}' | '\u{2029}' | '\u{85}' => {
                lines.push(std::mem::take(&mut current));
                col = 0;
            }
            '\t' => {
                let spaces = TAB_STOP - col % TAB_STOP;
                current.push_str(&" ".repeat(spaces));
                col += spaces;
            }
            '\x1b' => {
                skip_escape(&mut chars);
            }
            c if hidden_char(c) => {}
            c => {
                current.push(c);
                col += char_width(c);
            }
        }
    }
    lines.push(current);

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let line = line.trim_end().to_string();
        if line.is_empty() && out.last().map_or(true, |l: &String| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        Some(']') | Some('P') | Some('_') | Some('^') => {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\x07' || c == '\n' {
                    break;
                }
                if c == '\x1b' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

pub fn clean_inline(text: &str) -> String {
    clean_lines(text).join(" ")
}

pub fn wrap(line: &str, first: usize, rest: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut limit = first.max(2);
    while start < chars.len() {
        let mut used = 0usize;
        let mut end = start;
        let mut brk = None;
        while end < chars.len() {
            let w = char_width(chars[end]);
            if used + w > limit {
                break;
            }
            used += w;
            end += 1;
            if used * 2 >= limit && BREAK_AFTER.contains(&chars[end - 1]) {
                brk = Some(end);
            }
        }
        if end == start {
            end = start + 1;
        }
        let cut = if end < chars.len() {
            brk.unwrap_or(end)
        } else {
            end
        };
        let row: String = chars[start..cut].iter().collect();
        rows.push(row.trim_end().to_string());
        start = cut;
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
        limit = rest.max(2);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

pub fn fold(lines: &[String], width: usize, max_rows: usize, hang: usize) -> Folded {
    let width = width.max(4);
    let budget = max_rows.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut shown_chars = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let lead = line.len() - line.trim_start_matches(' ').len();
        let indent = (lead + hang).min(width / 2);
        let pieces = wrap(line, width, width - indent);
        for (j, piece) in pieces.iter().enumerate() {
            if rows.len() == budget {
                let last_row = i + 1 == lines.len() && j + 1 == pieces.len();
                if last_row {
                    rows.push(row_text(j, indent, piece));
                    return Folded { rows, more: None };
                }
                let more = if j == 0 || i + 1 < lines.len() {
                    More::Lines(lines.len() - i)
                } else {
                    More::Chars(line.chars().count().saturating_sub(shown_chars))
                };
                return Folded {
                    rows,
                    more: Some(more),
                };
            }
            if j == 0 {
                shown_chars = 0;
            }
            shown_chars += piece.chars().count();
            rows.push(row_text(j, indent, piece));
        }
    }
    Folded { rows, more: None }
}

fn row_text(index: usize, indent: usize, piece: &str) -> String {
    if index == 0 {
        piece.to_string()
    } else {
        format!("{}{piece}", " ".repeat(indent))
    }
}

pub fn fits(rows: &[String], width: usize) -> bool {
    rows.iter().all(|r| display_width(r) <= width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn line_endings_and_separators_all_split_lines() {
        assert_eq!(
            clean_lines("a\r\nb\rc\u{2028}d\ne"),
            owned(&["a", "b", "c", "d", "e"])
        );
    }

    #[test]
    fn tabs_expand_to_the_next_stop() {
        assert_eq!(clean_lines("\tx"), owned(&["    x"]));
        assert_eq!(clean_lines("ab\tx"), owned(&["ab  x"]));
        assert_eq!(clean_lines("\t\ty"), owned(&["        y"]));
    }

    #[test]
    fn escape_sequences_and_hidden_characters_are_dropped() {
        assert_eq!(clean_lines("\x1b[31mred\x1b[0m"), owned(&["red"]));
        assert_eq!(clean_lines("\x1b]0;title\x07after"), owned(&["after"]));
        assert_eq!(clean_lines("\x1b]8;;http://x\x1b\\link"), owned(&["link"]));
        assert_eq!(
            clean_lines("a\u{7}b\u{8}c\u{202E}d\u{200B}e\u{FEFF}f\u{0}"),
            owned(&["abcdef"])
        );
    }

    #[test]
    fn blank_lines_are_trimmed_and_collapsed() {
        assert_eq!(
            clean_lines("\n\n  \nfirst   \n\n\n\nsecond\n\n"),
            owned(&["first", "", "second"])
        );
        assert!(clean_lines(" \r\n\t\n").is_empty());
    }

    #[test]
    fn inline_text_joins_lines_with_spaces() {
        assert_eq!(clean_inline("one\r\ntwo\tthree"), "one two three");
    }

    #[test]
    fn long_lines_wrap_with_a_hanging_indent() {
        let line = "    foreach ($n in $list) { Write-Output $n.Name; Write-Output $n.Status }";
        let folded = fold(&owned(&[line]), 30, 12, 2);
        assert!(folded.more.is_none());
        assert!(folded.rows.len() > 1);
        assert!(folded.rows[0].starts_with("    foreach"));
        for row in &folded.rows[1..] {
            assert!(row.starts_with("      "));
            assert!(!row.starts_with("       "));
        }
        assert!(fits(&folded.rows, 30));
        let joined: String = folded
            .rows
            .iter()
            .map(|r| r.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, line.trim());
    }

    #[test]
    fn wrapping_prefers_spaces_and_separators() {
        assert_eq!(
            wrap("alpha beta gamma delta", 12, 12),
            owned(&["alpha beta", "gamma delta"])
        );
        assert_eq!(
            wrap(r"C:\Users\ADMIN\Documents\x", 16, 16),
            owned(&[r"C:\Users\ADMIN\", r"Documents\x"])
        );
        assert_eq!(wrap("abcdefghij", 4, 4), owned(&["abcd", "efgh", "ij"]));
        assert_eq!(wrap("", 10, 10), owned(&[""]));
    }

    #[test]
    fn rows_are_capped_with_a_count_of_what_is_left() {
        let lines: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        let folded = fold(&lines, 80, 12, 2);
        assert_eq!(folded.rows.len(), 12);
        assert_eq!(folded.rows[11], "line 12");
        assert_eq!(folded.more, Some(More::Lines(18)));
        assert_eq!(folded.more.unwrap().note(), "... 18 more lines");
        assert_eq!(More::Lines(1).note(), "... 1 more line");
        assert_eq!(More::Chars(1).note(), "... 1 more character");
    }

    #[test]
    fn one_extra_line_is_shown_instead_of_a_note() {
        let lines: Vec<String> = (1..=13).map(|i| format!("line {i}")).collect();
        let folded = fold(&lines, 80, 12, 2);
        assert_eq!(folded.rows.len(), 13);
        assert!(folded.more.is_none());
    }

    #[test]
    fn a_huge_single_line_counts_the_hidden_characters() {
        let line = "a".repeat(10_000);
        let folded = fold(&[line], 50, 12, 2);
        assert_eq!(folded.rows.len(), 12);
        assert!(fits(&folded.rows, 50));
        let shown: usize = folded.rows.iter().map(|r| r.trim().len()).sum();
        assert_eq!(folded.more, Some(More::Chars(10_000 - shown)));
    }

    #[test]
    fn a_cut_line_in_the_middle_counts_as_a_hidden_line() {
        let lines = owned(&["short", &"b".repeat(400), "after"]);
        let folded = fold(&lines, 40, 3, 0);
        assert_eq!(folded.rows.len(), 3);
        assert_eq!(folded.more, Some(More::Lines(2)));
    }

    #[test]
    fn wide_characters_never_overflow_the_row() {
        let text = "网络诊断🚀✅".repeat(20);
        for width in [5, 21, 40, 41] {
            let folded = fold(std::slice::from_ref(&text), width, 100, 0);
            assert!(fits(&folded.rows, width), "width {width}");
            assert!(folded.rows.iter().all(|r| !r.is_empty()));
            let rebuilt: String = folded.rows.concat();
            assert_eq!(rebuilt, text);
        }
    }

    #[test]
    fn a_tiny_width_still_makes_progress() {
        let folded = fold(&["网络".to_string()], 1, 10, 0);
        assert_eq!(folded.rows.concat(), "网络");
    }
}
