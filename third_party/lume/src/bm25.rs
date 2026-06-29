use std::collections::HashMap;
use serde::{Serialize, Deserialize};
use crate::tokenize;
use crate::fast_retrieval::{MiniRoaring, PrimeFilter};
use crate::Tagger;

/// Search-time diagnostics. Upstream lume prints two-stage pruning stats and
/// per-candidate rejection reasons to stderr on every query. When lume is
/// embedded as a library (nemesis8 session search) that noise is unwanted, so
/// these only fire when `LUME_DEBUG` is set in the environment.
macro_rules! dbg_log {
    ($($arg:tt)*) => {
        if std::env::var_os("LUME_DEBUG").is_some() {
            eprintln!($($arg)*);
        }
    };
}

/// Represents a section parsed from a Markdown document.
#[derive(Debug, Clone)]
pub struct Section {
    pub title: String,
    pub body: String,
    pub line_number: usize,
    pub filename: Option<String>,
}

/// The three BM25 variants supported by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchVariant {
    Classic,
    Plus,
    L,
}

/// Tuning parameters and field weights for the field-aware BM25 engine.
#[derive(Debug, Clone)]
pub struct Bm25Params {
    pub k1: f64,
    pub b: f64,
    pub delta: f64, // Used for BM25+
    pub title_weight: f64,
    pub body_weight: f64,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Self {
            k1: 1.2,
            b: 0.75,
            delta: 1.0,
            title_weight: 2.0,
            body_weight: 1.0,
        }
    }
}

/// A parsed, in-memory index of Markdown sections.
#[derive(Debug, Clone)]
pub struct Bm25Index {
    pub sections: Vec<Section>,
    pub num_docs: usize,
    
    // Per-document term frequency maps for each field (indexed by token bytes)
    pub title_tfs: Vec<HashMap<Vec<u8>, usize>>,
    pub body_tfs: Vec<HashMap<Vec<u8>, usize>>,
    
    // Total token counts per document field
    pub title_lens: Vec<usize>,
    pub body_lens: Vec<usize>,
    
    // Average field lengths across the corpus
    pub avg_title_len: f64,
    pub avg_body_len: f64,
    
    // Corpus-wide document frequencies: token bytes -> number of docs containing it
    pub title_dfs: HashMap<Vec<u8>, usize>,
    pub body_dfs: HashMap<Vec<u8>, usize>,

    // Native roaring bitmaps and prime/Gödel partitioned signature filters
    pub posting_lists: HashMap<Vec<u8>, MiniRoaring>,
    pub prime_filters: Vec<PrimeFilter>,
    pub tag_prime_map: HashMap<String, u128>,

    // Entity information for Semantic Mesh (Option A)
    pub entity_posting_lists: HashMap<String, MiniRoaring>,
    pub entity_kinds: HashMap<String, String>,
    pub entity_labels: HashMap<String, String>,
}

/// A hit returned by the search query.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub section_index: usize,
    pub score: f64,
}

/// Represents the reason why a candidate section was rejected during ranking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RejectReason {
    MissingSection,
    TagSignatureMismatch,
    NoTokenMatch,
    ScoreBelowThreshold(f64),
    EmptyText,
    FieldNotRankable,
}

/// Diagnostic information for a ranked candidate.
#[derive(Debug, Clone)]
pub struct RankDebug {
    pub section_id: u32,
    pub score: Option<f64>,
    pub rejected: Option<RejectReason>,
}

/// Simple, robust line-by-line Markdown section parser.
/// Cuts sections at `#` headers and records their starting line numbers.
pub fn parse_markdown(content: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut current_title = String::from("Introduction");
    let mut current_body = Vec::new();
    let mut start_line = 1;

    for (i, line) in content.lines().enumerate() {
        let line_num = i + 1;
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            let hashes_count = trimmed.chars().take_while(|&c| c == '#').count();
            let header_text = trimmed[hashes_count..].trim().to_string();
            
            if hashes_count > 0 && !header_text.is_empty() {
                // Save previous section if it has any content
                let body_text = current_body.join("\n");
                sections.push(Section {
                    title: current_title,
                    body: body_text,
                    line_number: start_line,
                    filename: None,
                });
                
                current_title = header_text;
                current_body.clear();
                start_line = line_num;
                continue;
            }
        }
        current_body.push(line.to_string());
    }

    // Push the final section
    let body_text = current_body.join("\n");
    sections.push(Section {
        title: current_title,
        body: body_text,
        line_number: start_line,
        filename: None,
    });

    // Retain sections that aren't completely blank
    sections.retain(|s| !s.title.trim().is_empty() || !s.body.trim().is_empty());
    sections
}

impl Bm25Index {
    /// Constructs a search index over a collection of Markdown sections.
    pub fn build(sections: Vec<Section>, tagger: Option<&Tagger>) -> Self {
        let mut tag_prime_map = HashMap::new();
        if let Some(t) = tagger {
            let mut unique_tags = std::collections::BTreeSet::new();
            for sec in &sections {
                for tag in t.tag(&sec.title) {
                    unique_tags.insert(tag.output.clone());
                }
                for tag in t.tag(&sec.body) {
                    unique_tags.insert(tag.output.clone());
                }
            }
            for (idx, tag_out) in unique_tags.into_iter().enumerate() {
                let prime = crate::fast_retrieval::get_nth_prime(idx + 1);
                tag_prime_map.insert(tag_out, prime);
            }
        }

        let num_docs = sections.len();
        let mut title_tfs = Vec::with_capacity(num_docs);
        let mut body_tfs = Vec::with_capacity(num_docs);
        let mut title_lens = Vec::with_capacity(num_docs);
        let mut body_lens = Vec::with_capacity(num_docs);
        
        let mut title_dfs = HashMap::new();
        let mut body_dfs = HashMap::new();
        
        let mut total_title_len = 0;
        let mut total_body_len = 0;

        let mut posting_lists: HashMap<Vec<u8>, MiniRoaring> = HashMap::new();
        let mut prime_filters = Vec::with_capacity(num_docs);

        let mut entity_posting_lists: HashMap<String, MiniRoaring> = HashMap::new();
        let mut entity_kinds = HashMap::new();
        let mut entity_labels = HashMap::new();

        for (doc_idx, sec) in sections.iter().enumerate() {
            let doc_id = doc_idx as u32;
            let t_toks = tokenize(&sec.title);
            let b_toks = tokenize(&sec.body);
            
            title_lens.push(t_toks.len());
            body_lens.push(b_toks.len());
            total_title_len += t_toks.len();
            total_body_len += b_toks.len();
            
            // Build Title TF
            let mut t_tf = HashMap::new();
            for tok in &t_toks {
                *t_tf.entry(tok.bytes.clone()).or_insert(0) += 1;
                posting_lists.entry(tok.bytes.clone()).or_default().insert(doc_id);
            }
            for tok_bytes in t_tf.keys() {
                *title_dfs.entry(tok_bytes.clone()).or_insert(0) += 1;
            }
            title_tfs.push(t_tf);
            
            // Build Body TF
            let mut b_tf = HashMap::new();
            for tok in &b_toks {
                *b_tf.entry(tok.bytes.clone()).or_insert(0) += 1;
                posting_lists.entry(tok.bytes.clone()).or_default().insert(doc_id);
            }
            for tok_bytes in b_tf.keys() {
                *body_dfs.entry(tok_bytes.clone()).or_insert(0) += 1;
            }
            body_tfs.push(b_tf);

            // Compute PrimeFilter signatures
            let mut pf = PrimeFilter::new();
            for tok in &t_toks {
                pf.add_term(&tok.bytes);
            }
            for tok in &b_toks {
                pf.add_term(&tok.bytes);
            }

            if let Some(t) = tagger {
                let title_tags = t.tag(&sec.title);
                for tag in title_tags {
                    if let Some(&prime) = tag_prime_map.get(&tag.output) {
                        pf.add_tag_prime(prime);
                    }
                    
                    // Track for semantic mesh (Option A)
                    entity_posting_lists
                        .entry(tag.output.clone())
                        .or_default()
                        .insert(doc_id);
                    entity_kinds.insert(tag.output.clone(), tag.kind.clone());
                    
                    // Keep the best version of the surface label (longer / capitalized)
                    let entry = entity_labels.entry(tag.output.clone());
                    match entry {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            v.insert(tag.surface.clone());
                        }
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            let curr = o.get();
                            let is_better = (tag.surface.chars().next().is_some_and(|c| c.is_uppercase()) &&
                                            !curr.chars().next().is_some_and(|c| c.is_uppercase())) ||
                                            tag.surface.len() > curr.len();
                            if is_better {
                                o.insert(tag.surface.clone());
                            }
                        }
                    }
                }
                let body_tags = t.tag(&sec.body);
                for tag in body_tags {
                    if let Some(&prime) = tag_prime_map.get(&tag.output) {
                        pf.add_tag_prime(prime);
                    }
                    
                    // Track for semantic mesh (Option A)
                    entity_posting_lists
                        .entry(tag.output.clone())
                        .or_default()
                        .insert(doc_id);
                    entity_kinds.insert(tag.output.clone(), tag.kind.clone());
                    
                    // Keep the best version of the surface label (longer / capitalized)
                    let entry = entity_labels.entry(tag.output.clone());
                    match entry {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            v.insert(tag.surface.clone());
                        }
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            let curr = o.get();
                            let is_better = (tag.surface.chars().next().is_some_and(|c| c.is_uppercase()) &&
                                            !curr.chars().next().is_some_and(|c| c.is_uppercase())) ||
                                            tag.surface.len() > curr.len();
                            if is_better {
                                o.insert(tag.surface.clone());
                            }
                        }
                    }
                }
            }
            prime_filters.push(pf);
        }
        
        let avg_title_len = if num_docs > 0 {
            total_title_len as f64 / num_docs as f64
        } else {
            0.0
        };
        
        let avg_body_len = if num_docs > 0 {
            total_body_len as f64 / num_docs as f64
        } else {
            0.0
        };

        Self {
            sections,
            num_docs,
            title_tfs,
            body_tfs,
            title_lens,
            body_lens,
            avg_title_len,
            avg_body_len,
            title_dfs,
            body_dfs,
            posting_lists,
            prime_filters,
            tag_prime_map,
            entity_posting_lists,
            entity_kinds,
            entity_labels,
        }
    }

    /// Evaluates a query and returns matching sections ordered by their BM25 score.
    pub fn search(
        &self,
        query: &str,
        variant: SearchVariant,
        params: &Bm25Params,
        tagger: Option<&Tagger>,
    ) -> Vec<SearchHit> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() || self.num_docs == 0 {
            return Vec::new();
        }
        
        let start_pruning = std::time::Instant::now();

        // 1. Gather all candidates using union of query term roaring bitmaps
        let mut candidate_set = MiniRoaring::new();
        let mut first = true;
        for q_tok in &query_tokens {
            if let Some(list) = self.posting_lists.get(&q_tok.bytes) {
                if first {
                    candidate_set = list.clone();
                    first = false;
                } else {
                    candidate_set = candidate_set.union(list);
                }
            }
        }

        let candidate_ids = candidate_set.iter();
        let num_candidates_roaring = candidate_ids.len();

        // 2. Further prune using Gödel tag signatures if query has tagged entities
        let mut query_tag_primes = Vec::new();
        if let Some(t) = tagger {
            let query_tags = t.tag(query);
            for tag in &query_tags {
                if let Some(&prime) = self.tag_prime_map.get(&tag.output) {
                    query_tag_primes.push(prime);
                } else {
                    let dummy_prime = crate::fast_retrieval::get_nth_prime(self.tag_prime_map.len() + 2);
                    query_tag_primes.push(dummy_prime);
                }
            }
        }

        let mut rejected_missing = 0;
        let mut rejected_empty = 0;
        let mut rejected_tag_mismatch = 0;
        let mut rejected_no_token = 0;
        let rejected_below_threshold = 0;
        let mut rejected_not_rankable = 0;

        let mut candidate_details = Vec::with_capacity(num_candidates_roaring);
        let mut pruned_candidates = Vec::with_capacity(num_candidates_roaring);

        for doc_id in candidate_ids {
            let doc_idx = doc_id as usize;
            if doc_idx >= self.sections.len() {
                rejected_missing += 1;
                candidate_details.push(RankDebug {
                    section_id: doc_id,
                    score: None,
                    rejected: Some(RejectReason::MissingSection),
                });
                continue;
            }

            let sec = &self.sections[doc_idx];
            if sec.title.is_empty() && sec.body.is_empty() {
                rejected_empty += 1;
                candidate_details.push(RankDebug {
                    section_id: doc_id,
                    score: None,
                    rejected: Some(RejectReason::EmptyText),
                });
                continue;
            }

            if self.title_lens[doc_idx] == 0 && self.body_lens[doc_idx] == 0 {
                rejected_not_rankable += 1;
                candidate_details.push(RankDebug {
                    section_id: doc_id,
                    score: None,
                    rejected: Some(RejectReason::FieldNotRankable),
                });
                continue;
            }

            let pf = &self.prime_filters[doc_idx];
            
            // Tag signature verification: Candidate must contain all query tag outputs if present
            let mut tag_match = true;
            for &prime in &query_tag_primes {
                if !pf.test_tag_prime(prime) {
                    tag_match = false;
                    break;
                }
            }
            if !tag_match {
                rejected_tag_mismatch += 1;
                candidate_details.push(RankDebug {
                    section_id: doc_id,
                    score: None,
                    rejected: Some(RejectReason::TagSignatureMismatch),
                });
                continue;
            }

            pruned_candidates.push(doc_id);
        }

        let pruning_elapsed = start_pruning.elapsed();
        dbg_log!(
            "\x1B[32m[Two-Stage Pruning] Pruned candidate space from {} to {} (roaring generated: {}) sections in {:.2?}\x1B[0m",
            self.num_docs, pruned_candidates.len(), num_candidates_roaring, pruning_elapsed
        );

        // Stage 2: Heavy Scoring on active candidates only
        let mut hits = Vec::new();
        
        for doc_id in pruned_candidates {
            let doc_idx = doc_id as usize;
            let mut total_score = 0.0;
            let pf = &self.prime_filters[doc_idx];
            
            for q_tok in &query_tokens {
                let tok_bytes = &q_tok.bytes;
                
                // 1. Title Contribution
                let title_score = {
                    // Check prime filter first for fast signature membership test
                    if pf.test_term(tok_bytes) {
                        let tf = self.title_tfs[doc_idx].get(tok_bytes).copied().unwrap_or(0) as f64;
                        if tf > 0.0 {
                            let df = self.title_dfs.get(tok_bytes).copied().unwrap_or(0);
                            
                            let idf = ((self.num_docs as f64 - df as f64 + 0.5) / (df as f64 + 0.5) + 1.0).ln();
                            let idf = idf.max(0.0);
                            
                            let doc_len = self.title_lens[doc_idx] as f64;
                            let avgdl = self.avg_title_len;
                            
                            calculate_bm25_term_score(tf, doc_len, avgdl, idf, variant, params)
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    }
                };
                
                // 2. Body Contribution
                let body_score = {
                    // Check prime filter first for fast signature membership test
                    if pf.test_term(tok_bytes) {
                        let tf = self.body_tfs[doc_idx].get(tok_bytes).copied().unwrap_or(0) as f64;
                        if tf > 0.0 {
                            let df = self.body_dfs.get(tok_bytes).copied().unwrap_or(0);
                            
                            let idf = ((self.num_docs as f64 - df as f64 + 0.5) / (df as f64 + 0.5) + 1.0).ln();
                            let idf = idf.max(0.0);
                            
                            let doc_len = self.body_lens[doc_idx] as f64;
                            let avgdl = self.avg_body_len;
                            
                            calculate_bm25_term_score(tf, doc_len, avgdl, idf, variant, params)
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    }
                };
                
                total_score += params.title_weight * title_score + params.body_weight * body_score;
            }
            
            if total_score > 0.0 {
                hits.push(SearchHit {
                    section_index: doc_idx,
                    score: total_score,
                });
                candidate_details.push(RankDebug {
                    section_id: doc_id,
                    score: Some(total_score),
                    rejected: None,
                });
            } else {
                rejected_no_token += 1;
                candidate_details.push(RankDebug {
                    section_id: doc_id,
                    score: Some(0.0),
                    rejected: Some(RejectReason::NoTokenMatch),
                });
            }
        }
        
        // Print high-level Rejection Accounting summary to stderr
        dbg_log!("\x1B[33mCandidates: {}\x1B[0m", num_candidates_roaring);
        dbg_log!("\x1B[33mRanked: {}\x1B[0m", hits.len());
        dbg_log!("\x1B[33mRejected:\x1B[0m");
        dbg_log!("  MissingSection: {}", rejected_missing);
        dbg_log!("  EmptyText: {}", rejected_empty);
        dbg_log!("  FieldNotRankable: {}", rejected_not_rankable);
        dbg_log!("  TagSignatureMismatch: {}", rejected_tag_mismatch);
        dbg_log!("  NoTokenMatch: {}", rejected_no_token);
        dbg_log!("  ScoreBelowThreshold: {}", rejected_below_threshold);

        // Trigger deep diagnostic explanation if hits is empty but we had candidates
        if hits.is_empty() && num_candidates_roaring > 0 {
            dbg_log!("\n\x1B[1;31m🔍 [Deep Rejection Diagnostics] Why zero ranked results?\x1B[0m");
            for detail in &candidate_details {
                if let Some(reason) = detail.rejected {
                    let doc_id = detail.section_id;
                    dbg_log!("  \x1B[1;33mCandidate {} rejected:\x1B[0m {:?}", doc_id, reason);
                    
                    let doc_idx = doc_id as usize;
                    if doc_idx < self.sections.len() {
                        let sec = &self.sections[doc_idx];
                        dbg_log!("     - Header: {:?}", sec.title);
                        dbg_log!("     - Body Snippet: {:?}", if sec.body.len() > 100 { format!("{}...", &sec.body[..100]) } else { sec.body.clone() });
                        
                        let title_tokens = tokenize(&sec.title);
                        let body_tokens = tokenize(&sec.body);
                        
                        let title_terms: Vec<String> = title_tokens.iter().map(|t| String::from_utf8_lossy(&t.bytes).to_string()).collect();
                        let body_terms: Vec<String> = body_tokens.iter().map(|t| String::from_utf8_lossy(&t.bytes).to_string()).collect();
                        
                        dbg_log!("     - Title Tokens: {:?}", title_terms);
                        dbg_log!("     - Body Tokens: {:?}", body_terms);
                        
                        let pf = &self.prime_filters[doc_idx];
                        
                        dbg_log!("     - Token-by-Token Query Evaluation:");
                        for q_tok in &query_tokens {
                            let term_str = String::from_utf8_lossy(&q_tok.bytes);
                            let prime_match = pf.test_term(&q_tok.bytes);
                            
                            let title_tf = self.title_tfs[doc_idx].get(&q_tok.bytes).copied().unwrap_or(0);
                            let body_tf = self.body_tfs[doc_idx].get(&q_tok.bytes).copied().unwrap_or(0);
                            
                            dbg_log!(
                                "       * Term '{}' -> Prime Filter Match: {} | Title TF: {} | Body TF: {}",
                                term_str, prime_match, title_tf, body_tf
                            );
                        }
                    }
                }
            }
            dbg_log!();
        }

        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits
    }
}

/// Helper function to perform BM25 variant score calculation.
fn calculate_bm25_term_score(
    tf: f64,
    doc_len: f64,
    avgdl: f64,
    idf: f64,
    variant: SearchVariant,
    params: &Bm25Params,
) -> f64 {
    if tf == 0.0 {
        return 0.0;
    }
    
    let k1 = params.k1;
    let b = params.b;
    
    let len_normalization = if avgdl > 0.0 {
        1.0 - b + b * (doc_len / avgdl)
    } else {
        1.0
    };
    
    match variant {
        SearchVariant::Classic => {
            idf * (tf * (k1 + 1.0)) / (tf + k1 * len_normalization)
        }
        SearchVariant::Plus => {
            let term_tf_score = (tf * (k1 + 1.0)) / (tf + k1 * len_normalization);
            idf * (term_tf_score + params.delta)
        }
        SearchVariant::L => {
            let scaled_tf = tf / len_normalization;
            idf * (scaled_tf * (k1 + 1.0)) / (scaled_tf + k1)
        }
    }
}
