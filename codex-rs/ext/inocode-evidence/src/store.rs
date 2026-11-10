use crate::model::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

pub struct NewClaim<'a> {
    pub text: &'a str,
    pub claim_type: ClaimType,
    pub status: &'a str,
    pub confidence: Option<Confidence>,
    pub span_ids: Vec<String>,
    pub premise_ids: Vec<String>,
    pub created_by: &'a str,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn write_json<T: Serialize>(&self, dir: &str, id: &str, v: &T) -> io::Result<()> {
        let d = self.dir(dir);
        std::fs::create_dir_all(&d)?;
        let text = serde_json::to_string_pretty(v).map_err(io::Error::other)?;
        std::fs::write(d.join(format!("{id}.json")), text)
    }

    fn load_all<T: DeserializeOwned>(&self, dir: &str) -> Vec<T> {
        let mut items: Vec<(String, T)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.dir(dir)) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(t) = std::fs::read_to_string(&p)
                    && let Ok(v) = serde_json::from_str::<T>(&t)
                {
                    items.push((
                        p.file_stem().unwrap_or_default().to_string_lossy().into(),
                        v,
                    ));
                }
            }
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items.into_iter().map(|(_, v)| v).collect()
    }

    fn next_id(&self, dir: &str, prefix: &str) -> String {
        let max = std::fs::read_dir(self.dir(dir))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_prefix(&format!("{prefix}-"))?
                    .split('.')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .max()
            .unwrap_or(0);
        format!("{prefix}-{:04}", max + 1)
    }

    // ---- 原資料 ----

    /// 原文(バイト列)を変更せずに保存する。
    pub fn import_bytes(
        &self,
        title: &str,
        kind: &str,
        bytes: &[u8],
        version: Option<&str>,
        origin: Option<&str>,
    ) -> io::Result<Source> {
        let id = self.next_id("sources", "src");
        let hash = format!("{:x}", Sha256::digest(bytes));
        let d = self.dir("sources");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join(format!("{id}.txt")), bytes)?;
        let src = Source {
            id: id.clone(),
            title: title.to_string(),
            kind: kind.to_string(),
            hash,
            retrieved_at: Self::now(),
            version: version.map(str::to_string),
            origin: origin.map(str::to_string),
        };
        self.write_json("sources", &id, &src)?;
        Ok(src)
    }

    pub fn import_text(
        &self,
        title: &str,
        kind: &str,
        text: &str,
        version: Option<&str>,
    ) -> io::Result<Source> {
        self.import_bytes(title, kind, text.as_bytes(), version, None)
    }

    pub fn import_file(
        &self,
        path: &Path,
        title: Option<&str>,
        kind: &str,
        version: Option<&str>,
    ) -> io::Result<Source> {
        let bytes = std::fs::read(path)?;
        let t = title
            .map(str::to_string)
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        self.import_bytes(&t, kind, &bytes, version, Some(&path.to_string_lossy()))
    }

    pub fn list_sources(&self) -> Vec<Source> {
        self.load_all("sources")
    }

    pub fn read_source(&self, id: &str) -> Option<(Source, String)> {
        let src = self.list_sources().into_iter().find(|s| s.id == id)?;
        let bytes = std::fs::read(self.dir("sources").join(format!("{id}.txt"))).ok()?;
        Some((src, String::from_utf8_lossy(&bytes).into_owned()))
    }

    // ---- 引用箇所 ----

    /// 原文の `[start, end)`(文字オフセット)を引用箇所にする。
    pub fn add_span(&self, source_id: &str, start: usize, end: usize) -> io::Result<Span> {
        let (_, text) = self
            .read_source(source_id)
            .ok_or_else(|| err(format!("source not found: {source_id}")))?;
        let quote: String = text
            .chars()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect();
        if quote.is_empty() {
            return Err(err("empty span"));
        }
        let n = quote.chars().count();
        self.save_span(source_id, start, start + n, quote)
    }

    /// 原文中で `quote` が一字一句現れる最初の位置を引用箇所にする。
    pub fn add_quote(&self, source_id: &str, quote: &str) -> io::Result<Span> {
        let (_, text) = self
            .read_source(source_id)
            .ok_or_else(|| err(format!("source not found: {source_id}")))?;
        let byte = text
            .find(quote)
            .filter(|_| !quote.is_empty())
            .ok_or_else(|| err("quote not found verbatim in source"))?;
        let start = text[..byte].chars().count();
        self.save_span(
            source_id,
            start,
            start + quote.chars().count(),
            quote.to_string(),
        )
    }

    fn save_span(
        &self,
        source_id: &str,
        start: usize,
        end: usize,
        text: String,
    ) -> io::Result<Span> {
        let id = self.next_id("spans", "span");
        let span = Span {
            id: id.clone(),
            source_id: source_id.to_string(),
            start,
            end,
            text,
        };
        self.write_json("spans", &id, &span)?;
        Ok(span)
    }

    pub fn list_spans(&self) -> Vec<Span> {
        self.load_all("spans")
    }

    // ---- 主張 ----

    pub fn add_claim(&self, n: NewClaim<'_>) -> io::Result<Claim> {
        let spans: HashSet<String> = self.list_spans().into_iter().map(|s| s.id).collect();
        if let Some(bad) = n.span_ids.iter().find(|s| !spans.contains(*s)) {
            return Err(err(format!("span not found: {bad}")));
        }
        let claims: HashSet<String> = self.list_claims(None).into_iter().map(|c| c.id).collect();
        if let Some(bad) = n.premise_ids.iter().find(|s| !claims.contains(*s)) {
            return Err(err(format!("premise claim not found: {bad}")));
        }
        let id = self.next_id("claims", "claim");
        let c = Claim {
            id: id.clone(),
            text: n.text.to_string(),
            claim_type: n.claim_type,
            status: n.status.to_string(),
            confidence: n.confidence,
            span_ids: n.span_ids,
            premise_ids: n.premise_ids,
            created_by: n.created_by.to_string(),
        };
        self.write_json("claims", &id, &c)?;
        Ok(c)
    }

    pub fn list_claims(&self, status: Option<&str>) -> Vec<Claim> {
        self.load_all::<Claim>("claims")
            .into_iter()
            .filter(|c| status.is_none_or(|s| c.status == s))
            .collect()
    }

    pub fn set_claim_status(&self, id: &str, status: &str) -> io::Result<()> {
        let mut c = self
            .list_claims(None)
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| err(format!("claim not found: {id}")))?;
        c.status = status.to_string();
        self.write_json("claims", id, &c)
    }

    /// 各主張が根拠に届いているか。
    /// - 主張自身: 引用箇所を持つ(かつ原資料が存在する)か、前提が 1 つ以上あって全部が
    ///   「根拠のある fact の主張」であること。反証済み(refuted)は常に false。
    /// - 他の主張の根拠(前提)として通るのは fact の主張だけ。inference は前提にできない。
    pub fn claim_support(&self) -> HashMap<String, bool> {
        let claims = self.list_claims(None);
        let by_id: HashMap<&str, &Claim> = claims.iter().map(|c| (c.id.as_str(), c)).collect();
        let sources: HashSet<String> = self.list_sources().into_iter().map(|s| s.id).collect();
        let valid_spans: HashSet<String> = self
            .list_spans()
            .into_iter()
            .filter(|s| sources.contains(&s.source_id))
            .map(|s| s.id)
            .collect();
        fn go<'a>(
            id: &'a str,
            by_id: &HashMap<&'a str, &'a Claim>,
            valid_spans: &HashSet<String>,
            memo: &mut HashMap<String, bool>,
            stack: &mut Vec<&'a str>,
        ) -> bool {
            if let Some(v) = memo.get(id) {
                return *v;
            }
            if stack.contains(&id) {
                return false;
            }
            let Some(c) = by_id.get(id).copied() else {
                return false;
            };
            stack.push(id);
            let v = if c.status == "refuted" {
                false
            } else if c.span_ids.iter().any(|s| valid_spans.contains(s)) {
                true
            } else {
                !c.premise_ids.is_empty()
                    && c.premise_ids.iter().all(|p| {
                        by_id
                            .get(p.as_str())
                            .is_some_and(|pc| pc.claim_type == ClaimType::Fact)
                            && go(p, by_id, valid_spans, memo, stack)
                    })
            };
            stack.pop();
            memo.insert(id.to_string(), v);
            v
        }
        let mut memo = HashMap::new();
        for c in &claims {
            go(&c.id, &by_id, &valid_spans, &mut memo, &mut Vec::new());
        }
        memo
    }

    pub fn graph(&self, root_claim_id: Option<&str>) -> Graph {
        let claims = self.list_claims(None);
        let support = self.claim_support();
        let spans = self.list_spans();
        let sources = self.list_sources();
        let span_by: HashMap<&str, &Span> = spans.iter().map(|s| (s.id.as_str(), s)).collect();
        let src_by: HashMap<&str, &Source> = sources.iter().map(|s| (s.id.as_str(), s)).collect();
        let claim_by: HashMap<&str, &Claim> = claims.iter().map(|c| (c.id.as_str(), c)).collect();

        let selected: Vec<&Claim> = match root_claim_id {
            None => claims.iter().collect(),
            Some(r) => {
                let mut out: Vec<&Claim> = Vec::new();
                let mut seen: HashSet<&str> = HashSet::new();
                let mut todo = vec![r];
                while let Some(id) = todo.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    if let Some(c) = claim_by.get(id).copied() {
                        out.push(c);
                        todo.extend(c.premise_ids.iter().map(String::as_str));
                    }
                }
                out
            }
        };
        let mut g = Graph::default();
        let mut span_ids: Vec<&str> = Vec::new();
        for c in &selected {
            g.nodes.push(GraphNode {
                id: c.id.clone(),
                kind: "claim".into(),
                label: c.text.clone(),
                status: c.status.clone(),
                supported: support.get(&c.id).copied().unwrap_or(false),
            });
            for s in &c.span_ids {
                g.edges.push(GraphEdge {
                    from: c.id.clone(),
                    to: s.clone(),
                    kind: "supports".into(),
                });
                if !span_ids.contains(&s.as_str()) {
                    span_ids.push(s);
                }
            }
            for p in &c.premise_ids {
                g.edges.push(GraphEdge {
                    from: c.id.clone(),
                    to: p.clone(),
                    kind: "premise".into(),
                });
            }
        }
        let mut src_ids: Vec<&str> = Vec::new();
        for sid in span_ids {
            let Some(sp) = span_by.get(sid).copied() else {
                continue;
            };
            let ok = src_by.contains_key(sp.source_id.as_str());
            g.nodes.push(GraphNode {
                id: sp.id.clone(),
                kind: "span".into(),
                label: sp.text.clone(),
                status: "quoted".into(),
                supported: ok,
            });
            g.edges.push(GraphEdge {
                from: sp.id.clone(),
                to: sp.source_id.clone(),
                kind: "cites".into(),
            });
            if ok && !src_ids.contains(&sp.source_id.as_str()) {
                src_ids.push(&sp.source_id);
            }
        }
        for sid in src_ids {
            let s = src_by[sid];
            g.nodes.push(GraphNode {
                id: s.id.clone(),
                kind: "source".into(),
                label: s.title.clone(),
                status: "stored".into(),
                supported: true,
            });
        }
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(d.path().join("evidence"));
        (d, s)
    }

    fn claim<'a>(
        text: &'a str,
        t: ClaimType,
        spans: Vec<String>,
        premises: Vec<String>,
    ) -> NewClaim<'a> {
        NewClaim {
            text,
            claim_type: t,
            status: "unverified",
            confidence: None,
            span_ids: spans,
            premise_ids: premises,
            created_by: "m",
        }
    }

    #[test]
    fn import_keeps_bytes_and_hash() {
        let (_d, s) = store();
        let text = "第235条 他人の財物を窃取した者は、窃盗の罪とする。\r\n";
        let src = s
            .import_text("刑法", "law", text, Some("2025-06-01"))
            .unwrap();
        let (meta, body) = s.read_source(&src.id).unwrap();
        assert_eq!(body, text);
        assert_eq!(meta.hash.len(), 64);
        assert_eq!(meta.version.as_deref(), Some("2025-06-01"));
    }

    #[test]
    fn spans_claims_and_inference_cannot_be_premise() {
        let (_d, s) = store();
        let src = s
            .import_text("資料", "text", "被告人は店内で商品を持ち去った。", None)
            .unwrap();
        let span = s.add_quote(&src.id, "商品を持ち去った").unwrap();
        assert_eq!(span.start, 7);
        assert!(s.add_quote(&src.id, "存在しない").is_err());
        let fact = s
            .add_claim(claim(
                "商品が持ち去られた",
                ClaimType::Fact,
                vec![span.id.clone()],
                vec![],
            ))
            .unwrap();
        let inf = s
            .add_claim(claim(
                "故意があった",
                ClaimType::Inference,
                vec![],
                vec![fact.id.clone()],
            ))
            .unwrap();
        let chained = s
            .add_claim(claim(
                "だから有罪",
                ClaimType::Interpretation,
                vec![],
                vec![inf.id.clone()],
            ))
            .unwrap();
        let sup = s.claim_support();
        assert!(sup[&fact.id]);
        assert!(sup[&inf.id]);
        assert!(!sup[&chained.id]);
        let g = s.graph(Some(&chained.id));
        assert!(!g.nodes.iter().find(|n| n.id == chained.id).unwrap().supported);
        s.set_claim_status(&fact.id, "refuted").unwrap();
        assert!(!s.claim_support()[&fact.id]);
    }
}
