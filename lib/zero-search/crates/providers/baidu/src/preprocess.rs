//! HTML preprocessing that reproduces the libxml2 (lxml) tree
//! semantics the Python parser was measured against.
//!
//! The measured Baidu pages use XML-style self-closing syntax on
//! ordinary HTML elements (`<i class="cos-icon ..."/>`,
//! `<div class="c-atom-afterclick-recomm-wrap"/>`). libxml2 honours
//! `/>` and closes those elements immediately. html5ever follows
//! the HTML5 spec and *ignores* the self-closing flag on non-void
//! elements, which makes following siblings nest inside them and
//! corrupts every subtree query.
//!
//! To parse the same tree as lxml, self-closing non-void start tags
//! are rewritten `<tag ... />` -> `<tag ...></tag>` before parsing.
//! Void elements are left alone (both parsers treat them as void),
//! and the contents of comments, CDATA sections, doctypes and the
//! raw-text elements (script/style/textarea/title) are never
//! touched. Attribute values (single- or double-quoted) are
//! respected, so a `/` inside a quoted value is not a self-close.

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style", "textarea", "title"];

fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0C' | b'\r')
}

fn find_subslice(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() || from > hay.len() - needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn find_close_tag(hay: &[u8], tag_lower: &[u8], from: usize) -> Option<usize> {
    // Case-insensitive search for "</tag".
    let mut i = from;
    while i + 2 + tag_lower.len() <= hay.len() {
        if hay[i] == b'<'
            && hay[i + 1] == b'/'
            && hay[i + 2..i + 2 + tag_lower.len()]
                .iter()
                .zip(tag_lower.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub fn preprocess_html(html: &str) -> String {
    let b = html.as_bytes();
    let n = b.len();
    let mut out: Vec<u8> = Vec::with_capacity(n + 64);
    let mut i = 0usize;
    while i < n {
        if b[i] != b'<' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        // Comment: <!-- ... -->
        if b[i..].starts_with(b"<!--") {
            let end = find_subslice(b, b"-->", i + 4).map(|p| p + 3).unwrap_or(n);
            out.extend_from_slice(&b[i..end]);
            i = end;
            continue;
        }
        // CDATA section (bogus in HTML, copied verbatim either way).
        if b[i..].starts_with(b"<![CDATA[") {
            let end = find_subslice(b, b"]]>", i + 9).map(|p| p + 3).unwrap_or(n);
            out.extend_from_slice(&b[i..end]);
            i = end;
            continue;
        }
        // Doctype / declaration / processing instruction / end tag:
        // copy through the next '>'.
        if i + 1 < n && (b[i + 1] == b'!' || b[i + 1] == b'?' || b[i + 1] == b'/') {
            let end = find_subslice(b, b">", i + 1).map(|p| p + 1).unwrap_or(n);
            out.extend_from_slice(&b[i..end]);
            i = end;
            continue;
        }
        // Start tag.
        if i + 1 < n && b[i + 1].is_ascii_alphabetic() {
            // Tag name.
            let mut j = i + 1;
            while j < n
                && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b'_' | b':' | b'.'))
            {
                j += 1;
            }
            let tag_name = &b[i + 1..j];
            let tag_lower: Vec<u8> = tag_name.iter().map(|c| c.to_ascii_lowercase()).collect();
            // Scan the rest of the tag with quote tracking; record a
            // '/' outside quotes that is followed only by whitespace
            // and then '>' (the self-closing slash).
            let mut quote: Option<u8> = None;
            let mut slash_pos: Option<usize> = None;
            let mut k = j;
            let mut tag_end: Option<usize> = None; // index of '>'
            while k < n {
                let c = b[k];
                match quote {
                    Some(q) => {
                        if c == q {
                            quote = None;
                        }
                    }
                    None => {
                        if c == b'"' || c == b'\'' {
                            quote = Some(c);
                            slash_pos = None;
                        } else if c == b'/' {
                            slash_pos = Some(k);
                        } else if c == b'>' {
                            tag_end = Some(k);
                            break;
                        } else if !is_ascii_ws(c) {
                            slash_pos = None;
                        }
                    }
                }
                k += 1;
            }
            let tag_end = match tag_end {
                Some(e) => e,
                None => {
                    // Unterminated tag: copy the rest verbatim.
                    out.extend_from_slice(&b[i..]);
                    i = n;
                    continue;
                }
            };
            let tag_str = String::from_utf8_lossy(&tag_lower).into_owned();
            let is_void = VOID_ELEMENTS.contains(&tag_str.as_str());
            let is_raw = RAW_TEXT_ELEMENTS.contains(&tag_str.as_str());
            match slash_pos {
                Some(sp) if !is_void => {
                    // Rewrite: drop the '/', close the tag, then emit
                    // an explicit end tag.
                    out.extend_from_slice(&b[i..sp]);
                    out.push(b'>');
                    out.extend_from_slice(b"</");
                    out.extend_from_slice(tag_name);
                    out.push(b'>');
                    i = tag_end + 1;
                    continue;
                }
                _ => {
                    out.extend_from_slice(&b[i..tag_end + 1]);
                    i = tag_end + 1;
                    if is_raw {
                        // Copy raw content up to the matching end
                        // tag (handled by the main loop afterwards).
                        match find_close_tag(b, &tag_lower, i) {
                            Some(close) => {
                                out.extend_from_slice(&b[i..close]);
                                i = close;
                            }
                            None => {
                                out.extend_from_slice(&b[i..]);
                                i = n;
                            }
                        }
                    }
                    continue;
                }
            }
        }
        // A '<' that starts nothing recognised: literal text.
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_self_closing_non_void() {
        assert_eq!(
            preprocess_html(r#"<div><i class="a"/><span>x</span></div>"#),
            r#"<div><i class="a"></i><span>x</span></div>"#
        );
        assert_eq!(
            preprocess_html(r#"<div class="c-atom-afterclick-recomm-wrap"/><p>y</p>"#),
            r#"<div class="c-atom-afterclick-recomm-wrap"></div><p>y</p>"#
        );
    }

    #[test]
    fn keeps_void_and_quoted_slashes() {
        assert_eq!(
            preprocess_html(r#"<link rel="preload" href="//x/y.js"/><br/><img src="a/b.png">"#),
            r#"<link rel="preload" href="//x/y.js"/><br/><img src="a/b.png">"#
        );
        assert_eq!(
            preprocess_html(r#"<div data-x="a/>b">t</div>"#),
            r#"<div data-x="a/>b">t</div>"#
        );
    }

    #[test]
    fn keeps_raw_text_and_comments() {
        let html = r#"<style>.a{content:"<i/>"}</style><!-- <div/> --><script>var s="<span/>";</script><i/>"#;
        let expected = r#"<style>.a{content:"<i/>"}</style><!-- <div/> --><script>var s="<span/>";</script><i></i>"#;
        assert_eq!(preprocess_html(html), expected);
    }
}
