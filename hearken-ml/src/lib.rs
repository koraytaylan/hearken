use hearken_core::{LogTemplate, tokenize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MlError {
    #[error("Parsing error: {0}")]
    Parse(String),
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

enum Node {
    Internal(HashMap<String, Node>),
    Leaf(Vec<usize>), // Indices into LogParser.templates
}

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
}
