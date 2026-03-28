use hearken_core::{LogTemplate, tokenize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MlError {
    #[error("Parsing error: {0}")]
    Parse(String),
}

/// Compare two template token vectors and return a similarity score in [0.0, 1.0].
/// Tokens must have the same length; `<*>` wildcards match anything.
/// Newline tokens must match exactly or similarity is 0.
pub fn template_similarity(a: &[String], b: &[String]) -> f64 {
    if a.len() != b.len() { return 0.0; }
    if a.is_empty() { return 1.0; }
    let mut matches = 0;
    for (ta, tb) in a.iter().zip(b.iter()) {
        if ta == "\n" || tb == "\n" {
            if ta != tb { return 0.0; }
            matches += 1;
            continue;
        }
        if ta == tb || ta == "<*>" || tb == "<*>" { matches += 1; }
    }
    matches as f64 / a.len() as f64
}

/// Computes semantic similarity between two template token vectors using cosine similarity.
/// Unlike `template_similarity`, this works across different token lengths.
/// `idf` maps token -> inverse document frequency weight (higher = rarer = more important).
/// Tokens appearing in >80% of templates get near-zero weight.
pub fn semantic_similarity(a: &[String], b: &[String], idf: &HashMap<String, f64>) -> f64 {
    let vec_a = tfidf_vector(a, idf);
    let vec_b = tfidf_vector(b, idf);

    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;

    let all_keys: HashSet<&String> = vec_a.keys().chain(vec_b.keys()).collect();
    for key in all_keys {
        let va = vec_a.get(key).copied().unwrap_or(0.0);
        let vb = vec_b.get(key).copied().unwrap_or(0.0);
        dot += va * vb;
        norm_a += va * va;
        norm_b += vb * vb;
    }

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

fn tfidf_vector(tokens: &[String], idf: &HashMap<String, f64>) -> HashMap<String, f64> {
    let mut tf: HashMap<String, f64> = HashMap::new();
    let count = tokens.len() as f64;
    if count == 0.0 { return tf; }

    for t in tokens {
        if t == "<*>" || t == "\n" { continue; }
        *tf.entry(t.clone()).or_insert(0.0) += 1.0;
    }

    for (token, freq) in tf.iter_mut() {
        *freq = (*freq / count) * idf.get(token).copied().unwrap_or(1.0);
    }
    tf
}

/// Computes IDF weights from a collection of template token vectors.
/// IDF = log(N / df) where df is the number of templates containing the token.
pub fn compute_idf(templates: &[Vec<String>]) -> HashMap<String, f64> {
    let n = templates.len() as f64;
    if n == 0.0 { return HashMap::new(); }

    let mut df: HashMap<String, usize> = HashMap::new();
    for tokens in templates {
        let unique: HashSet<&String> = tokens.iter().filter(|t| *t != "<*>" && *t != "\n").collect();
        for t in unique {
            *df.entry(t.clone()).or_insert(0) += 1;
        }
    }

    let mut idf = HashMap::new();
    for (token, count) in df {
        let weight = (n / count as f64).ln();
        if count as f64 / n > 0.8 {
            idf.insert(token, weight * 0.1);
        } else {
            idf.insert(token, weight);
        }
    }
    idf
}

#[derive(Clone, Debug)]
pub struct InternalTemplate {
    pub id: Option<i64>,
    pub tokens: Vec<String>,
}

impl InternalTemplate {
    /// Join tokens into a template string, converting `\n` tokens to real newlines.
    pub fn template_string(&self) -> String {
        let mut result = String::new();
        let mut after_newline = false;
        for token in &self.tokens {
            if token == "\n" {
                result.push('\n');
                after_newline = true;
            } else {
                if !result.is_empty() && !after_newline {
                    result.push(' ');
                }
                result.push_str(token);
                after_newline = false;
            }
        }
        result
    }

    pub fn to_log_template(&self) -> LogTemplate {
        LogTemplate { id: self.id, template: self.template_string() }
    }
}

#[derive(Clone)]
enum Node {
    Internal(HashMap<String, Node>),
    Leaf(Vec<usize>), // Indices into LogParser.templates
}

#[derive(Clone)]
pub struct LogParser {
    max_depth: usize,
    similarity_threshold: f64,
    root: HashMap<usize, Node>,
    pub templates: Vec<InternalTemplate>,
}

impl LogParser {
    pub fn new(max_depth: usize, similarity_threshold: f64) -> Self {
        Self {
            max_depth,
            similarity_threshold,
            root: HashMap::new(),
            templates: Vec::new(),
        }
    }

    #[inline(always)]
    fn is_variable(t: &str) -> bool {
        // Paths are always variables
        if t.as_bytes().iter().any(|&b| b == b'/' || b == b'\\') {
            return true;
        }

        let len = t.len();
        if len == 0 { return false; }

        let mut digits: usize = 0;
        let mut dashes: usize = 0;
        for b in t.bytes() {
            if b.is_ascii_digit() { digits += 1; }
            if b == b'-' { dashes += 1; }
        }

        // UUIDs: 2+ dashes and long (e.g. 550e8400-e29b-41d4-a716-446655440000)
        if dashes >= 2 && len > 10 { return true; }

        // Digit ratio: tokens that are predominantly numeric are variables
        // (timestamps, IPs, numeric IDs). Tokens where digits are incidental
        // (e.g. Class.method(File.java:538) — 5% digits) are NOT variables.
        if digits > 0 {
            return (digits * 100 / len) >= 30;
        }

        false
    }

    pub fn add_template(&mut self, template: LogTemplate) {
        // Round-trip: split on newlines first to reconstruct \n tokens
        let mut tokens: Vec<String> = Vec::new();
        for (i, line) in template.template.split('\n').enumerate() {
            if i > 0 {
                tokens.push("\n".to_string());
            }
            tokens.extend(tokenize(line).into_iter().map(|s| s.to_string()));
        }
        let token_count = tokens.len();
        if token_count == 0 { return; }
        
        let idx = self.templates.len();
        self.templates.push(InternalTemplate { id: template.id, tokens: tokens.clone() });

        let mut current_node = self.root.entry(token_count).or_insert_with(|| Node::Internal(HashMap::new()));
        let nav_limit = std::cmp::min(self.max_depth, token_count + 1);

        for depth in 1..nav_limit {
            let token = &tokens[depth - 1];
            let key = if Self::is_variable(token) { "<*>".to_string() } else { token.clone() };
            
            if let Node::Internal(children) = current_node {
                current_node = children.entry(key).or_insert_with(|| {
                    if depth + 1 == nav_limit { Node::Leaf(Vec::new()) } else { Node::Internal(HashMap::new()) }
                });
            }
        }
        
        if let Node::Leaf(candidates) = current_node {
            candidates.push(idx);
        }
    }

    /// High-speed immutable match for parallel processing using zero-allocation token slices
    pub fn find_match(&self, tokens: &[&str]) -> Option<usize> {
        let token_count = tokens.len();
        if token_count == 0 { return None; }
        
        let node = self.root.get(&token_count)?;
        let candidates = self.find_candidates(node, tokens, 1)?;
        
        const MAX_CANDIDATES: usize = 50;
        const EARLY_EXIT_THRESHOLD: f64 = 0.9;

        let mut best_match: Option<(usize, f64)> = None;
        for &idx in candidates.iter().take(MAX_CANDIDATES) {
            let candidate = &self.templates[idx];
            let sim = self.calculate_similarity(tokens, &candidate.tokens);
            if sim >= EARLY_EXIT_THRESHOLD {
                return Some(idx);
            }
            if sim >= self.similarity_threshold {
                if best_match.as_ref().map_or(true, |(_, best_sim)| sim > *best_sim) {
                    best_match = Some((idx, sim));
                }
            }
        }
        best_match.map(|(idx, _)| idx)
    }

    /// Parse tokens and return the template index.
    /// For unmatched lines, re-checks via the prefix tree (which includes templates
    /// added earlier in this batch) rather than a linear scan.
    pub fn parse_tokens(&mut self, tokens: &[&str], matched_idx: Option<usize>) -> usize {
        let token_count = tokens.len();
        if token_count == 0 { return usize::MAX; }

        let mut final_idx = matched_idx;

        // If parallel phase didn't find a match, re-check via the tree.
        // Templates added earlier in this sequential pass are already in the tree,
        // so find_match will find them efficiently via O(depth + candidates).
        if final_idx.is_none() {
            final_idx = self.find_match(tokens);
        }

        if let Some(idx) = final_idx {
            let candidate = &mut self.templates[idx];
            let mut changed = false;
            for i in 0..candidate.tokens.len() {
                if candidate.tokens[i] != tokens[i] && candidate.tokens[i] != "<*>" {
                    if candidate.tokens[i] == "\n" { continue; } // protect line boundaries
                    candidate.tokens[i] = "<*>".to_string();
                    changed = true;
                }
            }
            if changed && candidate.id.is_some() {
                // Mark as evolved by negating ID
                let id = candidate.id.unwrap();
                if id > 0 {
                    candidate.id = Some(-id);
                }
            }
            return idx;
        }

        let new_tokens = self.create_initial_tokens(tokens);
        let new_internal = InternalTemplate { id: None, tokens: new_tokens };
        let idx = self.templates.len();
        self.templates.push(new_internal);
        
        let mut current_node = self.root.entry(token_count).or_insert_with(|| Node::Internal(HashMap::new()));
        let nav_limit = std::cmp::min(self.max_depth, token_count + 1);

        for depth in 1..nav_limit {
            let token = tokens[depth - 1];
            let key = if Self::is_variable(token) { "<*>".to_string() } else { token.to_string() };
            
            if let Node::Internal(children) = current_node {
                current_node = children.entry(key).or_insert_with(|| {
                    if depth + 1 == nav_limit { Node::Leaf(Vec::new()) } else { Node::Internal(HashMap::new()) }
                });
            }
        }
        
        if let Node::Leaf(leaf) = current_node {
            leaf.push(idx);
            idx
        } else {
            unreachable!()
        }
    }

    fn find_candidates<'a>(&'a self, node: &'a Node, tokens: &[&str], depth: usize) -> Option<&'a Vec<usize>> {
        let nav_limit = std::cmp::min(self.max_depth, tokens.len() + 1);
        if depth >= nav_limit {
            return if let Node::Leaf(candidates) = node { Some(candidates) } else { None };
        }

        match node {
            Node::Internal(children) => {
                let token = tokens[depth - 1];
                let key = if Self::is_variable(token) { "<*>" } else { token };
                children.get(key).or_else(|| children.get("<*>"))
                    .and_then(|next| self.find_candidates(next, tokens, depth + 1))
            }
            Node::Leaf(candidates) => Some(candidates),
        }
    }

    fn calculate_similarity(&self, tokens: &[&str], template_tokens: &[String]) -> f64 {
        if tokens.len() != template_tokens.len() { return 0.0; }
        let mut matches = 0;
        for (t, temp_t) in tokens.iter().zip(template_tokens.iter()) {
            // Newline structure must match exactly — entries with different
            // continuation line boundaries must never merge into the same pattern
            if *t == "\n" || temp_t == "\n" {
                if *t != temp_t.as_str() { return 0.0; }
                matches += 1;
                continue;
            }
            if *t == temp_t || temp_t == "<*>" { matches += 1; }
        }
        matches as f64 / tokens.len() as f64
    }

    fn create_initial_tokens(&self, tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| {
            if Self::is_variable(t) { "<*>".to_string() } else { t.to_string() }
        }).collect()
    }

    pub fn extract_variables_from_tokens<'a>(tokens: &[&'a str], template_tokens: &[String]) -> Vec<&'a str> {
        tokens.iter().zip(template_tokens.iter())
            .filter(|(_, t)| *t == "<*>")
            .map(|(l, _)| *l)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_parser() -> LogParser {
        LogParser::new(15, 0.5)
    }

    #[test]
    fn test_is_variable_digit_ratio() {
        // Low digit ratio — NOT variable (class/method names preserved)
        assert!(!LogParser::is_variable("com.example.app.Widget.process(Widget.java:538)"));
        assert!(!LogParser::is_variable("[com.example.module.resolver:1.7.10.B004]"));
        assert!(!LogParser::is_variable("AbstractFilterChain.doFilter(AbstractFilterChain.java:78)"));

        // High digit ratio — IS variable (timestamps, IPs)
        assert!(LogParser::is_variable("2025-03-15"));
        assert!(LogParser::is_variable("192.168.1.100"));
        assert!(LogParser::is_variable("14:05:33.710"));

        // Paths always variable
        assert!(LogParser::is_variable("/data/reports/summary.html"));
        assert!(LogParser::is_variable("C:\\logs\\app.log"));

        // UUIDs
        assert!(LogParser::is_variable("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn test_newline_tokens_never_variable() {
        assert!(!LogParser::is_variable("\n"));
    }

    #[test]
    fn test_similarity_rejects_newline_mismatch() {
        let parser = make_parser();
        let template = vec!["at".into(), "Foo.bar()".into(), "\n".into(), "at".into(), "Baz.qux()".into()];
        // Same \n position — should match
        let tokens = vec!["at", "Other.method()", "\n", "at", "Another.call()"];
        assert!(parser.calculate_similarity(&tokens, &template) > 0.0);

        // \n at different position — should reject
        let tokens_bad = vec!["at", "\n", "Foo.bar()", "at", "Baz.qux()"];
        assert_eq!(parser.calculate_similarity(&tokens_bad, &template), 0.0);
    }

    #[test]
    fn test_newline_tokens_protected_from_wildcarding() {
        let mut parser = make_parser();
        let tokens1: Vec<&str> = vec!["msg", "hello", "\n", "at", "Foo.bar()"];
        parser.parse_tokens(&tokens1, None);
        assert_eq!(parser.templates[0].tokens[2], "\n");

        // Simulate parallel-phase match (matched_idx = Some(0)) to force evolution
        let tokens2: Vec<&str> = vec!["msg", "world", "\n", "at", "Baz.qux()"];
        parser.parse_tokens(&tokens2, Some(0));
        assert_eq!(parser.templates[0].tokens[2], "\n");   // \n protected from wildcarding
        assert_eq!(parser.templates[0].tokens[1], "<*>");   // "hello" → "world" evolved
        assert_eq!(parser.templates[0].tokens[4], "<*>");   // "Foo.bar()" → "Baz.qux()" evolved
    }

    #[test]
    fn test_template_string_preserves_newlines() {
        let tmpl = InternalTemplate {
            id: None,
            tokens: vec![
                "msg".into(), "hello".into(), "\n".into(),
                "at".into(), "Foo.bar()".into(), "\n".into(),
                "at".into(), "Baz.qux()".into(),
            ],
        };
        assert_eq!(tmpl.template_string(), "msg hello\nat Foo.bar()\nat Baz.qux()");
    }

    #[test]
    fn test_add_template_roundtrip_with_newlines() {
        let mut parser = make_parser();
        parser.add_template(LogTemplate {
            id: Some(1),
            template: "msg hello\nat Foo.bar()\nat Baz.qux()".to_string(),
        });
        assert_eq!(parser.templates[0].tokens, vec!["msg", "hello", "\n", "at", "Foo.bar()", "\n", "at", "Baz.qux()"]);
        assert_eq!(parser.templates[0].template_string(), "msg hello\nat Foo.bar()\nat Baz.qux()");
    }

    #[test]
    fn test_different_continuation_structures_separate_patterns() {
        let mut parser = make_parser();
        // 2-token frames
        let t1: Vec<&str> = vec!["error", "\n", "at", "Foo.bar()", "\n", "at", "Baz.qux()"];
        parser.parse_tokens(&t1, None);
        assert_eq!(parser.templates.len(), 1);

        // 3-token frames (same total count, \n at different positions)
        let t2: Vec<&str> = vec!["error", "\n", "at", "Foo.bar()", "[module:1.0]", "\n", "at"];
        parser.parse_tokens(&t2, None);
        assert_eq!(parser.templates.len(), 2);
    }

    #[test]
    fn test_semantic_similarity_identical() {
        let idf = HashMap::from([("User".to_string(), 1.0), ("logged".to_string(), 1.0), ("in".to_string(), 0.5)]);
        let a = vec!["User".to_string(), "<*>".to_string(), "logged".to_string(), "in".to_string()];
        let b = a.clone();
        assert!((semantic_similarity(&a, &b, &idf) - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_semantic_similarity_different_lengths() {
        let idf = HashMap::from([
            ("connection".to_string(), 2.0),
            ("refused".to_string(), 1.5),
            ("timeout".to_string(), 1.5),
            ("from".to_string(), 0.3),
        ]);
        let a = vec!["connection".to_string(), "refused".to_string()];
        let b = vec!["connection".to_string(), "timeout".to_string(), "from".to_string(), "<*>".to_string()];
        let sim = semantic_similarity(&a, &b, &idf);
        assert!(sim > 0.3, "Should have some similarity due to shared 'connection': {}", sim);
        assert!(sim < 0.9, "Should not be too similar: {}", sim);
    }

    #[test]
    fn test_compute_idf() {
        let templates = vec![
            vec!["User".to_string(), "<*>".to_string(), "logged".to_string(), "in".to_string()],
            vec!["User".to_string(), "<*>".to_string(), "logged".to_string(), "out".to_string()],
            vec!["Connection".to_string(), "refused".to_string()],
        ];
        let idf = compute_idf(&templates);
        // "User" appears in 2/3 templates, "Connection" in 1/3
        assert!(idf["Connection"] > idf["User"], "Rarer token should have higher IDF");
    }
}
