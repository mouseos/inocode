use crate::model::Finding;
use crate::model::FindingKind;
use regex::Regex;
use std::collections::HashSet;

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '０'..='９' => char::from(b'0' + (c as u32 - '０' as u32) as u8),
            '，' => ',',
            c => c,
        })
        .collect()
}

fn char_pos(text: &str, byte: usize) -> usize {
    text[..byte].chars().count()
}

/// 機械的な検証だけ(LLM は呼ばない)。検出して知らせるだけで、出力は止めない。
/// `sources` は指定した原資料の本文。
pub fn verify_document(text: &str, sources: &[String]) -> Vec<Finding> {
    let norm_sources: Vec<String> = sources.iter().map(|s| normalize(s)).collect();
    let in_sources = |needle: &str| {
        let n = normalize(needle);
        !n.is_empty() && norm_sources.iter().any(|s| s.contains(&n))
    };
    let mut findings = Vec::new();

    // (a) 引用
    let quote_re = Regex::new(r#"「([^」]+)」|“([^”]+)”|"([^"\n]+)""#).unwrap();
    let mut quote_ranges: Vec<(usize, usize)> = Vec::new();
    for m in quote_re.captures_iter(text) {
        let whole = m.get(0).unwrap();
        quote_ranges.push((whole.start(), whole.end()));
        let inner = m.get(1).or(m.get(2)).or(m.get(3)).unwrap();
        if !in_sources(inner.as_str()) {
            findings.push(Finding {
                kind: FindingKind::QuoteMissing,
                text: inner.as_str().to_string(),
                position: char_pos(text, inner.start()),
            });
        }
    }
    // `> ` の引用ブロック(連続する行をまとめて 1 つの引用にする)
    let mut offset = 0usize;
    let mut block: Option<(usize, String)> = None;
    let mut block_ranges: Vec<(usize, usize)> = Vec::new();
    let flush = |b: Option<(usize, String)>, findings: &mut Vec<Finding>| {
        if let Some((start, q)) = b
            && !q.trim().is_empty()
            && !in_sources(&q)
        {
            findings.push(Finding {
                kind: FindingKind::QuoteMissing,
                text: q.trim().to_string(),
                position: char_pos(text, start),
            });
        }
    };
    for line in text.split_inclusive('\n') {
        let t = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = t.strip_prefix('>') {
            let content = rest.strip_prefix(' ').unwrap_or(rest);
            block_ranges.push((offset, offset + line.len()));
            match &mut block {
                Some((_, q)) => {
                    q.push('\n');
                    q.push_str(content);
                }
                None => block = Some((offset + (t.len() - content.len()), content.to_string())),
            }
        } else {
            flush(block.take(), &mut findings);
        }
        offset += line.len();
    }
    flush(block.take(), &mut findings);

    // (b) 出典のない文
    let marker = Regex::new(
        r"\[[^\]]*\]|［[^］]*］|【[^】]*】|（出典|\(出典|出典[:：]|src-\d+|span-\d+|claim-\d+",
    )
    .unwrap();
    let sentence_re = Regex::new(r"[^。．!?！？\n]+[。．!?！？]?").unwrap();
    for m in sentence_re.find_iter(text) {
        let s = m.as_str();
        let trimmed = s.trim();
        if trimmed.chars().count() < 5 || trimmed.starts_with('#') {
            continue;
        }
        let in_quote = quote_ranges
            .iter()
            .chain(block_ranges.iter())
            .any(|(a, b)| m.start() < *b && m.end() > *a);
        if in_quote || marker.is_match(s) {
            continue;
        }
        findings.push(Finding {
            kind: FindingKind::Uncited,
            text: trimmed.to_string(),
            position: char_pos(text, m.start() + (s.len() - s.trim_start().len())),
        });
    }

    // (c) 新出語
    let mut seen: HashSet<String> = HashSet::new();
    let mut novel =
        |term: &str, byte: usize, check: &dyn Fn(&str) -> bool, findings: &mut Vec<Finding>| {
            if !seen.insert(term.to_string()) || check(term) {
                return;
            }
            findings.push(Finding {
                kind: FindingKind::NovelTerm,
                text: term.to_string(),
                position: char_pos(text, byte),
            });
        };
    let exact = |t: &str| in_sources(t);
    // 罪名: 末尾側 3 文字以上の語が原資料にあれば既出とみなす(「事実上窃盗罪」など)。
    let crime = |t: &str| {
        let chars: Vec<char> = t.chars().collect();
        (0..chars.len().saturating_sub(2))
            .any(|i| in_sources(&chars[i..].iter().collect::<String>()))
            || in_sources(t)
    };
    let generic = ["犯罪", "有罪", "無罪", "重罪", "罪"];
    let crime_re = Regex::new(r"\p{Han}{1,6}罪").unwrap();
    for m in crime_re.find_iter(text) {
        if generic.contains(&m.as_str()) {
            continue;
        }
        novel(m.as_str(), m.start(), &crime, &mut findings);
    }
    let article_re =
        Regex::new(r"第[0-9０-９一二三四五六七八九十百千]+条(?:の[0-9０-９一二三四五六七八九十]+)?")
            .unwrap();
    for m in article_re.find_iter(text) {
        novel(m.as_str(), m.start(), &exact, &mut findings);
    }
    let money_re =
        Regex::new(r"(?:[0-9０-９][0-9０-９,，.]*|[一二三四五六七八九十百千万億]+)(?:万|億|千)?円")
            .unwrap();
    for m in money_re.find_iter(text) {
        novel(m.as_str(), m.start(), &exact, &mut findings);
    }
    let date_re =
        Regex::new(r"(?:令和|平成|昭和)?[0-9０-９元]+年(?:[0-9０-９]+月)?(?:[0-9０-９]+日)?")
            .unwrap();
    for m in date_re.find_iter(text) {
        novel(m.as_str(), m.start(), &exact, &mut findings);
    }
    let name_re =
        Regex::new(r"(\p{Han}{2,4})(?:氏|さん|容疑者|被告人|被告|原告|弁護士|裁判官)").unwrap();
    let stop = ["本件", "当該", "上記", "同", "該", "各", "両", "前", "元"];
    for m in name_re.captures_iter(text) {
        let name = m.get(1).unwrap();
        if stop.contains(&name.as_str()) {
            continue;
        }
        novel(name.as_str(), name.start(), &exact, &mut findings);
    }
    findings.sort_by_key(|f| f.position);
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(f: &[Finding], k: FindingKind) -> Vec<String> {
        f.iter()
            .filter(|x| x.kind == k)
            .map(|x| x.text.clone())
            .collect()
    }

    #[test]
    fn novel_crime_name_is_detected() {
        let src = vec!["被告人は、他人の財物を奪い、強盗罪に当たる行為をした。".to_string()];
        let doc = "本件は窃盗罪に当たる。[src-0001]";
        let f = verify_document(doc, &src);
        assert_eq!(kinds(&f, FindingKind::NovelTerm), vec!["窃盗罪"]);
    }

    #[test]
    fn known_terms_pass() {
        let src = vec!["第235条は窃盗罪を定める。罰金は50万円。令和6年4月1日に施行。".to_string()];
        let doc = "第235条の窃盗罪について、50万円の罰金がある[src-0001]。令和6年4月1日。";
        let f = verify_document(doc, &src);
        assert!(kinds(&f, FindingKind::NovelTerm).is_empty(), "{f:?}");
    }

    #[test]
    fn novel_article_amount_date_name() {
        let src = vec!["条文は存在する。".to_string()];
        let f = verify_document("第246条により100万円、2020年5月3日、山田氏。[出典: x]", &src);
        let n = kinds(&f, FindingKind::NovelTerm);
        for t in ["第246条", "100万円", "2020年5月3日", "山田"] {
            assert!(n.contains(&t.to_string()), "{t} in {n:?}");
        }
    }

    #[test]
    fn quotes_and_uncited() {
        let src = vec!["財物を窃取した者は処罰される".to_string()];
        let doc =
            "裁判所は「財物を 窃取した者」と述べた[src-0001]。\n> 存在しない文\n根拠のない断定である。";
        let f = verify_document(doc, &src);
        assert_eq!(kinds(&f, FindingKind::QuoteMissing), vec!["存在しない文"]);
        assert_eq!(
            kinds(&f, FindingKind::Uncited),
            vec!["根拠のない断定である。"]
        );
    }
}
