use crate::fts::expand::{expand_fuzzy, expand_wildcard};
use crate::fts::query::{parse, QueryGroup, SubPattern};
use crate::fts::snippet::SnippetBuilder;
use crate::fts::tokenizer::HebrewTokenizer;
#[cfg(not(test))]
use crate::frb_generated::StreamSink;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use log::debug;
use std::collections::HashMap;
use tantivy::collector::{Collector, Count, FacetCollector, SegmentCollector, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::indexer::NoMergePolicy;
use tantivy::query::{BooleanQuery, Occur, TermQuery, TermSetQuery};
use tantivy::query::Query;
use tantivy::schema::Value;
use tantivy::{doc, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher};
use tantivy::{schema::*, Index};
use tantivy::{DocId, SegmentOrdinal, SegmentReader, Term};

// ── Public data types ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SearchResult {
    pub title: String,
    pub reference: String,
    pub text: String,
    pub id: u64,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
    pub score: u32,
    pub word_distance: u32,
}

pub struct DocumentInput {
    pub id: u64,
    pub title: String,
    pub reference: String,
    pub topics: String,
    pub text: String,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
}

pub struct HighlightConfig {
    pub highlight_prefix: String,
    pub highlight_postfix: String,
    pub max_chars: u32,
}

pub struct SearchPageResult {
    pub total_count: u32,
    pub results: Vec<SearchResult>,
}

pub struct FacetCount {
    pub path: String,
    pub count: u64,
}

pub enum ResultsOrder {
    Catalogue,
    Relevance,
}

// ── SearchEngine ───────────────────────────────────────────────────────────────

const DEFAULT_WRITER_HEAP_SIZE: usize = 50_000_000;

pub struct SearchEngine {
    schema: Schema,
    index: Index,
    index_writer: Option<IndexWriter>,
    writer_heap_size: usize,
    index_reader: IndexReader,
}

impl SearchEngine {
    #[frb(sync)]
    pub fn new(path: &str) -> Self {
        debug!("new path={}", path);
        let mut schema_builder = Schema::builder();
        let text_options = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("hebrew")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored()
            .set_fast(None);
        schema_builder.add_text_field("text", text_options);
        schema_builder.add_text_field("reference", STORED);
        schema_builder.add_text_field(
            "title",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("raw")
                        .set_fieldnorms(false),
                )
                .set_stored(),
        );
        schema_builder.add_u64_field("id", STORED | FAST | INDEXED);
        schema_builder.add_u64_field("segment", STORED);
        schema_builder.add_bool_field("isPdf", STORED);
        schema_builder.add_text_field("filePath", STRING | FAST | STORED);
        schema_builder.add_facet_field("topics", FacetOptions::default());

        let schema = schema_builder.build();
        let mmap_directory = MmapDirectory::open(path).expect("unable to open mmap directory");
        let index =
            Index::open_or_create(mmap_directory, schema.clone()).expect("Failed to create index");
        index.tokenizers().register("hebrew", HebrewTokenizer);
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .expect("Failed to create index reader");
        let index_writer = index
            .writer(DEFAULT_WRITER_HEAP_SIZE)
            .expect("Failed to create index writer");

        SearchEngine {
            schema,
            index,
            index_writer: Some(index_writer),
            writer_heap_size: DEFAULT_WRITER_HEAP_SIZE,
            index_reader,
        }
    }

    // ── Write API ──────────────────────────────────────────────────────────────

    pub fn add_document(
        &mut self,
        _id: u64,
        _title: &str,
        _reference: &str,
        _topics: &str,
        _text: &str,
        _segment: u64,
        _is_pdf: bool,
        _file_path: &str,
    ) -> Result<()> {
        let (title_f, reference_f, text_f, id_f, segment_f, is_pdf_f, file_path_f, topics_f) =
            self.all_fields()?;
        let topics_facet = Facet::from_text(_topics)?;
        self.writer_mut()?.add_document(doc!(
            title_f     => _title,
            reference_f => _reference,
            text_f      => _text,
            id_f        => _id,
            segment_f   => _segment,
            is_pdf_f    => _is_pdf,
            file_path_f => _file_path,
            topics_f    => topics_facet
        ))?;
        Ok(())
    }

    pub fn add_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        let (title_f, reference_f, text_f, id_f, segment_f, is_pdf_f, file_path_f, topics_f) =
            self.all_fields()?;
        let writer = self.writer_mut()?;
        for doc in docs {
            let topics_facet = Facet::from_text(&doc.topics)?;
            writer.add_document(doc!(
                title_f     => doc.title,
                reference_f => doc.reference,
                text_f      => doc.text,
                id_f        => doc.id,
                segment_f   => doc.segment,
                is_pdf_f    => doc.is_pdf,
                file_path_f => doc.file_path,
                topics_f    => topics_facet
            ))?;
        }
        Ok(())
    }

    pub fn upsert_document(
        &mut self,
        _id: u64,
        _title: &str,
        _reference: &str,
        _topics: &str,
        _text: &str,
        _segment: u64,
        _is_pdf: bool,
        _file_path: &str,
    ) -> Result<()> {
        self.delete_document_by_id(_id)?;
        self.add_document(
            _id, _title, _reference, _topics, _text, _segment, _is_pdf, _file_path,
        )
    }

    pub fn upsert_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        let (title_f, reference_f, text_f, id_f, segment_f, is_pdf_f, file_path_f, topics_f) =
            self.all_fields()?;
        let writer = self.writer_mut()?;
        for doc in docs {
            writer.delete_term(Term::from_field_u64(id_f, doc.id));
            let topics_facet = Facet::from_text(&doc.topics)?;
            writer.add_document(doc!(
                title_f     => doc.title,
                reference_f => doc.reference,
                text_f      => doc.text,
                id_f        => doc.id,
                segment_f   => doc.segment,
                is_pdf_f    => doc.is_pdf,
                file_path_f => doc.file_path,
                topics_f    => topics_facet
            ))?;
        }
        Ok(())
    }

    pub fn delete_document_by_id(&mut self, id: u64) -> Result<()> {
        let id_f = self.schema.get_field("id").unwrap();
        self.writer_mut()?
            .delete_term(Term::from_field_u64(id_f, id));
        Ok(())
    }

    pub fn remove_documents_by_title(&mut self, title: &str) -> Result<()> {
        let title_field = self.schema.get_field("title")?;
        self.writer_mut()?
            .delete_term(Term::from_field_text(title_field, title));
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        self.writer_mut()?.delete_all_documents()?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer_mut()?.commit()?;
        self.index_reader.reload()?;
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<()> {
        self.writer_mut()?.rollback()?;
        Ok(())
    }

    // ── Search API ─────────────────────────────────────────────────────────────

    pub fn search(
        &mut self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<Vec<SearchResult>> {
        let searcher = self.index_reader.searcher();
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        let (tantivy_query, expanded_groups) =
            self.build_fts_query(&searcher, &query, &facets)?;
        if tantivy_query.is_none() {
            return Ok(vec![]);
        }
        let tantivy_query = tantivy_query.unwrap();
        let addresses = Self::collect_addresses(&searcher, &*tantivy_query, limit, offset, &order)?;
        Self::build_results(&self.schema, &searcher, addresses, &expanded_groups, &hl)
    }

    pub fn search_and_count(
        &mut self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<SearchPageResult> {
        let searcher = self.index_reader.searcher();
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        let (tantivy_query, expanded_groups) =
            self.build_fts_query(&searcher, &query, &facets)?;
        if tantivy_query.is_none() {
            return Ok(SearchPageResult { total_count: 0, results: vec![] });
        }
        let tantivy_query = tantivy_query.unwrap();

        let (addresses, total_count): (Vec<DocAddress>, u32) = match order {
            ResultsOrder::Catalogue => {
                let top_collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_fast_field::<u64>("id", Order::Asc);
                let (top_docs, count) = searcher.search(&*tantivy_query, &(top_collector, Count))?;
                let addrs = top_docs.into_iter().map(|(_, addr)| addr).collect();
                (addrs, count as u32)
            }
            ResultsOrder::Relevance => {
                let top_collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_score();
                let (top_docs, count) = searcher.search(&*tantivy_query, &(top_collector, Count))?;
                let addrs = top_docs.into_iter().map(|(_, addr)| addr).collect();
                (addrs, count as u32)
            }
        };

        let results = Self::build_results(&self.schema, &searcher, addresses, &expanded_groups, &hl)?;
        Ok(SearchPageResult { total_count, results })
    }

    pub fn count(
        &mut self,
        query: String,
        facets: Vec<String>,
    ) -> Result<u32> {
        let searcher = self.index_reader.searcher();
        let (tantivy_query, _) = self.build_fts_query(&searcher, &query, &facets)?;
        if tantivy_query.is_none() {
            return Ok(0);
        }
        let tantivy_query = tantivy_query.unwrap();
        Ok(searcher.search(&*tantivy_query, &Count)? as u32)
    }

    pub fn count_by_book(
        &mut self,
        query: String,
        facets: Vec<String>,
    ) -> Result<HashMap<String, u32>> {
        let searcher = self.index_reader.searcher();
        let (tantivy_query, _) = self.build_fts_query(&searcher, &query, &facets)?;
        if tantivy_query.is_none() {
            return Ok(HashMap::new());
        }
        let tantivy_query = tantivy_query.unwrap();
        Ok(searcher.search(&*tantivy_query, &BookCountCollector)?)
    }

    pub fn get_facet_counts(
        &mut self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
    ) -> Result<Vec<FacetCount>> {
        let searcher = self.index_reader.searcher();
        let (tantivy_query, _) = self.build_fts_query(&searcher, &query, &facets)?;
        if tantivy_query.is_none() {
            return Ok(vec![]);
        }
        let tantivy_query = tantivy_query.unwrap();
        let mut facet_collector = FacetCollector::for_field("topics");
        facet_collector.add_facet(&facet_prefix);
        let facet_counts = searcher.search(&*tantivy_query, &facet_collector)?;
        let results = facet_counts
            .get(facet_prefix.as_str())
            .map(|(f, count)| FacetCount {
                path: f.to_string(),
                count,
            })
            .collect();
        Ok(results)
    }

    #[cfg(not(test))]
    pub fn search_stream(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
        chunk_size: u32,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let searcher = self.index_reader.searcher();
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        let (tantivy_query, expanded_groups) =
            self.build_fts_query_stateless(&searcher, &query, &facets)?;
        if tantivy_query.is_none() {
            return Ok(());
        }
        let tantivy_query = tantivy_query.unwrap();
        let chunk_size = (chunk_size.max(1)) as usize;

        let addresses = Self::collect_addresses(&searcher, &*tantivy_query, limit, offset, &order)?;

        for chunk in addresses.chunks(chunk_size) {
            let results =
                Self::build_results(&self.schema, &searcher, chunk.to_vec(), &expanded_groups, &hl)?;
            if sink.add(results).is_err() {
                break;
            }
        }
        Ok(())
    }

    // ── Operational API ────────────────────────────────────────────────────────

    pub fn optimize(&mut self) -> Result<()> {
        let before_count = self.index.searchable_segment_ids()?.len();
        debug!("optimize: before={before_count}");
        if before_count <= 1 {
            debug!("optimize: skipped");
            return Ok(());
        }

        let writer = self.take_writer()?;
        let maintenance_result = (|| -> Result<()> {
            writer.wait_merging_threads()?;
            self.optimize_committed_segments()
        })();
        let restore_result = self.restore_writer();

        if let Err(restore_err) = restore_result {
            return match maintenance_result {
                Ok(_) => Err(restore_err),
                Err(maintenance_err) => Err(restore_err.context(format!(
                    "optimize maintenance also failed: {maintenance_err:#}"
                ))),
            };
        }

        maintenance_result?;
        self.index_reader.reload()?;
        let after_count = self.index.searchable_segment_ids()?.len();
        debug!("optimize: after={after_count}");
        Ok(())
    }

    pub fn get_document_count(&self) -> u64 {
        self.index_reader.searcher().num_docs()
    }

    pub fn get_segment_count(&self) -> Result<u32> {
        Ok(self.index.searchable_segment_ids()?.len() as u32)
    }

    pub fn get_document_by_id(&self, id: u64) -> Result<Option<SearchResult>> {
        let id_f = self.schema.get_field("id")?;
        let term = Term::from_field_u64(id_f, id);
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let searcher = self.index_reader.searcher();

        let top_docs = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, addr)) = top_docs.into_iter().next() else {
            return Ok(None);
        };

        let doc = searcher.doc::<TantivyDocument>(addr)?;
        let title_f = self.schema.get_field("title")?;
        let reference_f = self.schema.get_field("reference")?;
        let text_f = self.schema.get_field("text")?;
        let segment_f = self.schema.get_field("segment")?;
        let is_pdf_f = self.schema.get_field("isPdf")?;
        let file_path_f = self.schema.get_field("filePath")?;

        Ok(Some(SearchResult {
            title: doc.get_first(title_f).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            reference: doc.get_first(reference_f).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            text: doc.get_first(text_f).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            id,
            segment: doc.get_first(segment_f).and_then(|v| v.as_u64()).unwrap_or_default(),
            is_pdf: doc.get_first(is_pdf_f).and_then(|v| v.as_bool()).unwrap_or_default(),
            file_path: doc.get_first(file_path_f).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            score: 0,
            word_distance: 0,
        }))
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn all_fields(&self) -> Result<(Field, Field, Field, Field, Field, Field, Field, Field)> {
        Ok((
            self.schema.get_field("title")?,
            self.schema.get_field("reference")?,
            self.schema.get_field("text")?,
            self.schema.get_field("id")?,
            self.schema.get_field("segment")?,
            self.schema.get_field("isPdf")?,
            self.schema.get_field("filePath")?,
            self.schema.get_field("topics")?,
        ))
    }

    fn ensure_writer(&mut self) -> Result<()> {
        if self.index_writer.is_none() {
            debug!("writer: reopening lazily");
            self.index_writer = Some(self.open_writer()?);
        }
        Ok(())
    }

    fn writer_mut(&mut self) -> Result<&mut IndexWriter> {
        self.ensure_writer()?;
        self.index_writer
            .as_mut()
            .context("index writer is not available")
    }

    fn take_writer(&mut self) -> Result<IndexWriter> {
        self.ensure_writer()?;
        self.index_writer
            .take()
            .context("index writer is not available")
    }

    fn open_writer(&self) -> Result<IndexWriter> {
        Ok(self.index.writer(self.writer_heap_size)?)
    }

    fn open_writer_no_merge(&self) -> Result<IndexWriter> {
        let writer = self.open_writer()?;
        writer.set_merge_policy(Box::new(NoMergePolicy));
        Ok(writer)
    }

    fn optimize_committed_segments(&self) -> Result<()> {
        let mut maintenance_writer = self.open_writer_no_merge()?;
        let segment_ids = self.index.searchable_segment_ids()?;
        debug!("optimize: merging {} segments", segment_ids.len());

        let merge_result = if segment_ids.len() > 1 {
            maintenance_writer.merge(&segment_ids).wait().map(|_| ())
        } else {
            Ok(())
        };
        let wait_result = maintenance_writer.wait_merging_threads();

        merge_result?;
        wait_result?;
        Ok(())
    }

    fn restore_writer(&mut self) -> Result<()> {
        self.index_writer = Some(self.open_writer()?);
        Ok(())
    }

    /// Parse the query string, expand terms, and build a Tantivy BooleanQuery.
    /// Returns (None, _) when the query is empty or a fuzzy term has no candidates (hard miss).
    /// `expanded_groups[i]` = concrete terms for AND-slot i (for snippet highlighting).
    fn build_fts_query(
        &self,
        searcher: &Searcher,
        query: &str,
        facets: &[String],
    ) -> Result<(Option<Box<dyn Query>>, Vec<Vec<String>>)> {
        self.build_fts_query_stateless(searcher, query, facets)
    }

    fn build_fts_query_stateless(
        &self,
        searcher: &Searcher,
        query: &str,
        facets: &[String],
    ) -> Result<(Option<Box<dyn Query>>, Vec<Vec<String>>)> {
        let parsed = parse(query);
        if parsed.is_empty() {
            return Ok((None, vec![]));
        }

        let text_field = self.schema.get_field("text")?;
        let topics_field = self.schema.get_field("topics")?;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut expanded_groups: Vec<Vec<String>> = Vec::new();

        for group in &parsed.groups {
            let group_terms = expand_group(group, text_field, searcher);
            if group_terms.is_none() {
                // Hard miss: fuzzy alternative returned no candidates
                return Ok((None, vec![]));
            }
            let group_terms = group_terms.unwrap();
            if group_terms.is_empty() {
                // Wildcard with no matches — skip this group
                continue;
            }

            let term_set: Vec<Term> = group_terms
                .iter()
                .map(|t| Term::from_field_text(text_field, t))
                .collect();

            clauses.push((Occur::Must, Box::new(TermSetQuery::new(term_set))));
            expanded_groups.push(group_terms);
        }

        if clauses.is_empty() {
            return Ok((None, vec![]));
        }

        // Facet filter
        let facet_terms: Vec<Term> = facets
            .iter()
            .map(|f| Term::from_facet(topics_field, &Facet::from_text(f).unwrap()))
            .collect();
        clauses.push((Occur::Must, Box::new(TermSetQuery::new(facet_terms))));

        let query: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
        Ok((Some(query), expanded_groups))
    }

    fn collect_addresses(
        searcher: &Searcher,
        query: &dyn Query,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
    ) -> Result<Vec<DocAddress>> {
        let addresses = match order {
            ResultsOrder::Catalogue => {
                let collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_fast_field::<u64>("id", Order::Asc);
                searcher
                    .search(query, &collector)?
                    .into_iter()
                    .map(|(_, addr)| addr)
                    .collect()
            }
            ResultsOrder::Relevance => {
                let collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_score();
                searcher
                    .search(query, &collector)?
                    .into_iter()
                    .map(|(_, addr)| addr)
                    .collect()
            }
        };
        Ok(addresses)
    }

    fn build_results(
        schema: &Schema,
        searcher: &Searcher,
        addresses: Vec<DocAddress>,
        expanded_groups: &[Vec<String>],
        hl: &HighlightConfig,
    ) -> Result<Vec<SearchResult>> {
        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        let text_field = schema.get_field("text")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;

        let snippet_builder = SnippetBuilder::new(
            &hl.highlight_prefix,
            &hl.highlight_postfix,
            hl.max_chars as usize,
            150,
        );

        let mut results = Vec::with_capacity(addresses.len());
        for doc_address in addresses {
            let retrieved_doc = match searcher.doc::<TantivyDocument>(doc_address) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let title = retrieved_doc.get_first(title_field).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let reference = retrieved_doc.get_first(reference_field).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let text = retrieved_doc.get_first(text_field).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let id = retrieved_doc.get_first(id_field).and_then(|v| v.as_u64()).unwrap_or_default();
            let segment = retrieved_doc.get_first(segment_field).and_then(|v| v.as_u64()).unwrap_or_default();
            let is_pdf = retrieved_doc.get_first(is_pdf_field).and_then(|v| v.as_bool()).unwrap_or_default();
            let file_path = retrieved_doc.get_first(file_path_field).and_then(|v| v.as_str()).unwrap_or_default().to_string();

            let snippet = snippet_builder.build(&text, expanded_groups);
            let result_text = if snippet.is_match && !snippet.html.is_empty() {
                snippet.html
            } else {
                text
            };

            results.push(SearchResult {
                title,
                reference,
                text: result_text,
                id,
                segment,
                is_pdf,
                file_path,
                score: if snippet.score == u32::MAX { 0 } else { snippet.score },
                word_distance: if snippet.word_distance == u32::MAX { 0 } else { snippet.word_distance },
            });
        }
        Ok(results)
    }
}

impl HighlightConfig {
    fn default() -> Self {
        HighlightConfig {
            highlight_prefix: "<font color=red>".to_string(),
            highlight_postfix: "</font>".to_string(),
            max_chars: 800,
        }
    }
}

// ── Query group expansion ──────────────────────────────────────────────────────

/// Expand all OR alternatives in a group into a flat deduplicated list of concrete terms.
/// Returns None on a hard miss (fuzzy alternative produced no candidates).
/// Returns Some(empty) when wildcards produced nothing (caller should skip the group).
fn expand_group(group: &QueryGroup, field: Field, searcher: &Searcher) -> Option<Vec<String>> {
    // Fast path: single literal
    if group.is_single() {
        let alt = &group.alternatives[0];
        if !alt.is_wildcard && !alt.is_fuzzy {
            return Some(vec![alt.pattern.clone()]);
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut result: Vec<String> = Vec::new();

    for alt in &group.alternatives {
        let expanded = expand_alternative(alt, field, searcher)?;
        for term in expanded {
            if seen.insert(term.clone()) {
                result.push(term);
            }
        }
    }

    Some(result)
}

/// Expand a single SubPattern into concrete terms.
/// Returns None only on a fuzzy hard miss.
fn expand_alternative(alt: &SubPattern, field: Field, searcher: &Searcher) -> Option<Vec<String>> {
    if alt.is_fuzzy {
        let terms = expand_fuzzy(&alt.pattern, alt.fuzzy_distance, field, searcher);
        if terms.is_empty() {
            // Hard miss: fuzzy term with no candidates in index
            return None;
        }
        return Some(terms);
    }

    if alt.is_wildcard {
        let terms = expand_wildcard(&alt.pattern, field, searcher);
        // Wildcard with no matches: return empty (caller skips group, not hard miss)
        return Some(terms);
    }

    // Literal
    Some(vec![alt.pattern.clone()])
}

// ── BookCountCollector ─────────────────────────────────────────────────────────

struct BookCountCollector;

struct BookCountSegmentCollector {
    str_col: Option<tantivy::columnar::StrColumn>,
    counts: HashMap<u64, u32>,
}

impl Collector for BookCountCollector {
    type Fruit = HashMap<String, u32>;
    type Child = BookCountSegmentCollector;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<BookCountSegmentCollector> {
        let str_col = reader.fast_fields().str("filePath")?;
        Ok(BookCountSegmentCollector {
            str_col,
            counts: HashMap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        per_segment: Vec<tantivy::Result<HashMap<String, u32>>>,
    ) -> tantivy::Result<HashMap<String, u32>> {
        let mut merged: HashMap<String, u32> = HashMap::new();
        for seg_result in per_segment {
            for (path, count) in seg_result? {
                *merged.entry(path).or_insert(0) += count;
            }
        }
        Ok(merged)
    }
}

impl SegmentCollector for BookCountSegmentCollector {
    type Fruit = tantivy::Result<HashMap<String, u32>>;

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        if let Some(col) = &self.str_col {
            if let Some(term_ord) = col.term_ords(doc_id).next() {
                *self.counts.entry(term_ord).or_insert(0) += 1;
            }
        }
    }

    fn harvest(self) -> tantivy::Result<HashMap<String, u32>> {
        let Some(col) = self.str_col else {
            return Ok(HashMap::new());
        };
        let mut result = HashMap::with_capacity(self.counts.len());
        let mut buf = String::new();
        for (term_ord, count) in self.counts {
            buf.clear();
            if col.ord_to_str(term_ord, &mut buf)? {
                result.insert(buf.clone(), count);
            }
        }
        Ok(result)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_engine() -> (SearchEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = SearchEngine::new(dir.path().to_str().unwrap());
        (engine, dir)
    }

    fn add(engine: &mut SearchEngine, id: u64, text: &str, file_path: &str) {
        engine
            .add_document(id, "title", "ref", "/root", text, 0, false, file_path)
            .unwrap();
    }

    fn disable_auto_merge(engine: &SearchEngine) {
        engine
            .index_writer
            .as_ref()
            .unwrap()
            .set_merge_policy(Box::new(NoMergePolicy));
    }

    fn search_ids(engine: &mut SearchEngine, query: &str) -> Vec<u64> {
        engine
            .search(
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap()
            .into_iter()
            .map(|result| result.id)
            .collect()
    }

    #[test]
    fn test_literal_search() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "ביי", "/books/b.txt");
        engine.commit().unwrap();

        let ids = search_ids(&mut engine, "שלום");
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[test]
    fn test_and_search() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        engine.commit().unwrap();

        let ids = search_ids(&mut engine, "שלום עולם");
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn test_or_search() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "ביי חבר", "/books/b.txt");
        add(&mut engine, 3, "אחר", "/books/c.txt");
        engine.commit().unwrap();

        let ids = search_ids(&mut engine, "שלום | ביי");
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn test_prefix_wildcard() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        add(&mut engine, 2, "שלומי", "/books/b.txt");
        add(&mut engine, 3, "ביי", "/books/c.txt");
        engine.commit().unwrap();

        let ids = search_ids(&mut engine, "שלו*");
        assert!(ids.contains(&1), "שלום should match שלו*");
        assert!(ids.contains(&2), "שלומי should match שלו*");
        assert!(!ids.contains(&3), "ביי should not match שלו*");
    }

    #[test]
    fn test_fuzzy_search() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        // "שלוף" shares trigram "שלו" with "שלום" — passes n-gram filter
        // and is 1 edit away (substitute ם→ף).
        add(&mut engine, 2, "שלוף", "/books/b.txt");
        add(&mut engine, 3, "ביי", "/books/c.txt");
        engine.commit().unwrap();

        // שלום~ should match שלום (exact) and שלוף (1 edit away)
        let ids = search_ids(&mut engine, "שלום~");
        assert!(ids.contains(&1), "exact match expected");
        assert!(ids.contains(&2), "one-edit match expected");
        assert!(!ids.contains(&3), "unrelated should not match");
    }

    #[test]
    fn test_count_by_book_basic() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book("שלום".to_string(), vec!["/root".to_string()])
            .unwrap();

        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
    }

    #[test]
    fn test_delete_document_by_id() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        engine.commit().unwrap();

        assert_eq!(engine.count("שלום".to_string(), vec!["/root".to_string()]).unwrap(), 2);

        engine.delete_document_by_id(1).unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.count("שלום".to_string(), vec!["/root".to_string()]).unwrap(), 1);
    }

    #[test]
    fn test_upsert_document() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "טקסט ישן", "/books/a.txt");
        engine.commit().unwrap();

        engine
            .upsert_document(1, "title", "ref", "/root", "טקסט חדש", 0, false, "/books/a.txt")
            .unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.count("טקסט".to_string(), vec!["/root".to_string()]).unwrap(), 1);
        assert_eq!(engine.count("ישן".to_string(), vec!["/root".to_string()]).unwrap(), 0);
        assert_eq!(engine.count("חדש".to_string(), vec!["/root".to_string()]).unwrap(), 1);
    }

    #[test]
    fn test_rollback() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();

        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        engine.rollback().unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.count("שלום".to_string(), vec!["/root".to_string()]).unwrap(), 1);
    }

    #[test]
    fn test_get_document_count() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        add(&mut engine, 2, "עולם", "/books/b.txt");
        engine.commit().unwrap();
        assert_eq!(engine.get_document_count(), 2);
    }

    #[test]
    fn test_get_document_by_id_found() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 42, "תורה ומצוות", "/books/a.txt");
        engine.commit().unwrap();

        let result = engine.get_document_by_id(42).unwrap();
        assert!(result.is_some());
        let doc = result.unwrap();
        assert_eq!(doc.id, 42);
        assert_eq!(doc.text, "תורה ומצוות");
    }

    #[test]
    fn test_get_document_by_id_not_found() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        let result = engine.get_document_by_id(999).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_search_and_count() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "ביי", "/books/b.txt");
        engine.commit().unwrap();

        let page = engine
            .search_and_count(
                "שלום".to_string(),
                vec!["/root".to_string()],
                1,
                0,
                ResultsOrder::Relevance,
                None,
            )
            .unwrap();

        assert_eq!(page.total_count, 2);
        assert_eq!(page.results.len(), 1);
    }

    #[test]
    fn test_search_offset() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/b.txt");
        add(&mut engine, 3, "שלום חבר", "/books/c.txt");
        engine.commit().unwrap();

        let page1 = engine.search("שלום".to_string(), vec!["/root".to_string()], 2, 0, ResultsOrder::Catalogue, None).unwrap();
        let page2 = engine.search("שלום".to_string(), vec!["/root".to_string()], 2, 2, ResultsOrder::Catalogue, None).unwrap();

        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 1);
        let ids1: Vec<u64> = page1.iter().map(|r| r.id).collect();
        let ids2: Vec<u64> = page2.iter().map(|r| r.id).collect();
        assert!(ids1.iter().all(|id| !ids2.contains(id)));
    }

    #[test]
    fn test_optimize_reduces_segments_many_commits() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);

        for id in 1..=12 {
            let text = format!("שלום {id}");
            let file_path = format!("/books/{id}.txt");
            add(&mut engine, id, &text, &file_path);
            engine.commit().unwrap();
        }

        let before = engine.get_segment_count().unwrap();
        assert!(before > 1);

        engine.optimize().unwrap();

        let after = engine.get_segment_count().unwrap();
        assert!(after <= before);
        assert_eq!(after, 1);
        assert_eq!(engine.get_document_count(), 12);
    }

    #[test]
    fn test_optimize_preserves_search_results() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);

        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "שלום רב", "/books/b.txt");
        engine.commit().unwrap();
        add(&mut engine, 3, "ביי", "/books/c.txt");
        engine.commit().unwrap();
        add(&mut engine, 4, "שלום חבר", "/books/d.txt");
        engine.commit().unwrap();

        let before_ids = search_ids(&mut engine, "שלום");
        engine.optimize().unwrap();
        let after_ids = search_ids(&mut engine, "שלום");

        assert_eq!(before_ids, after_ids);
    }

    #[test]
    fn test_optimize_preserves_upsert_and_delete_afterwards() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);

        add(&mut engine, 1, "טקסט ישן", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "למחיקה", "/books/b.txt");
        engine.commit().unwrap();

        engine.optimize().unwrap();

        engine.upsert_document(1, "title", "ref", "/root", "טקסט חדש", 0, false, "/books/a.txt").unwrap();
        engine.delete_document_by_id(2).unwrap();
        engine.commit().unwrap();

        assert_eq!(search_ids(&mut engine, "ישן"), Vec::<u64>::new());
        assert_eq!(search_ids(&mut engine, "חדש"), vec![1]);
        assert!(engine.get_document_by_id(2).unwrap().is_none());
    }

    #[test]
    fn test_optimize_noop_when_single_segment() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        let before = engine.get_segment_count().unwrap();
        engine.optimize().unwrap();
        let after = engine.get_segment_count().unwrap();

        assert_eq!(before, 1);
        assert_eq!(after, 1);

        add(&mut engine, 2, "עולם", "/books/b.txt");
        engine.commit().unwrap();
        assert_eq!(search_ids(&mut engine, "עולם"), vec![2]);
    }

    #[test]
    fn test_writer_reopens_after_transient_reopen_failure() {
        let (mut engine, _dir) = make_engine();

        engine.index_writer = None;
        let competing_writer: IndexWriter<TantivyDocument> =
            engine.index.writer(DEFAULT_WRITER_HEAP_SIZE).unwrap();

        let err = engine
            .add_document(1, "title", "ref", "/root", "שלום", 0, false, "/books/a.txt")
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to acquire index lock")
                || err.to_string().contains("LockFailure"),
            "unexpected error: {err:#}"
        );
        assert!(engine.index_writer.is_none());

        drop(competing_writer);

        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        assert_eq!(search_ids(&mut engine, "שלום"), vec![1]);
    }

    #[test]
    fn test_clear_reopens_after_transient_reopen_failure() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        engine.index_writer = None;
        let competing_writer: IndexWriter<TantivyDocument> =
            engine.index.writer(DEFAULT_WRITER_HEAP_SIZE).unwrap();

        let err = engine.clear().unwrap_err();
        assert!(
            err.to_string().contains("Failed to acquire index lock")
                || err.to_string().contains("LockFailure"),
            "unexpected error: {err:#}"
        );
        assert!(engine.index_writer.is_none());

        drop(competing_writer);

        engine.clear().unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.get_document_count(), 0);
        assert_eq!(search_ids(&mut engine, "שלום"), Vec::<u64>::new());
    }

    #[test]
    fn test_count_by_book_multi_segment() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book("שלום".to_string(), vec!["/root".to_string()])
            .unwrap();

        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
    }

    #[test]
    fn test_nikud_stripped_at_index_time() {
        let (mut engine, _dir) = make_engine();
        // Indexed text contains nikud + cantillation
        add(&mut engine, 1, "שָׁלוֹם עוֹלָם", "/books/a.txt");
        add(&mut engine, 2, "ביי", "/books/b.txt");
        engine.commit().unwrap();

        // Query without nikud must still match the nikud-bearing document
        let ids = search_ids(&mut engine, "שלום");
        assert_eq!(ids, vec![1]);

        let ids = search_ids(&mut engine, "עולם");
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn test_html_stripped_at_index_time() {
        let (mut engine, _dir) = make_engine();
        // Indexed text contains HTML markup
        add(&mut engine, 1, "<p>שלום <b>עולם</b></p>", "/books/a.txt");
        add(&mut engine, 2, "<div>חבר</div>", "/books/b.txt");
        engine.commit().unwrap();

        let ids = search_ids(&mut engine, "שלום");
        assert_eq!(ids, vec![1]);

        let ids = search_ids(&mut engine, "עולם");
        assert_eq!(ids, vec![1]);

        let ids = search_ids(&mut engine, "חבר");
        assert_eq!(ids, vec![2]);

        // Tag names must NOT be indexed
        let ids = search_ids(&mut engine, "div");
        assert!(ids.is_empty());
    }

    #[test]
    fn test_optional_char_through_search_api() {
        let (mut engine, _dir) = make_engine();
        // "שלום" and "שלם" — the ? should match both
        add(&mut engine, 1, "שלום", "/books/a.txt");
        add(&mut engine, 2, "שלם", "/books/b.txt");
        add(&mut engine, 3, "ביי", "/books/c.txt");
        engine.commit().unwrap();

        // "שלו?ם" → ו is optional → matches both "שלום" and "שלם"
        let ids = search_ids(&mut engine, "שלו?ם");
        assert!(ids.contains(&1), "שלום should match שלו?ם");
        assert!(ids.contains(&2), "שלם should match שלו?ם");
        assert!(!ids.contains(&3));
    }

    #[test]
    fn test_snippet_strips_html_tags() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "<p>שלום <b>עולם</b> חבר</p>", "/books/a.txt");
        engine.commit().unwrap();

        let results = engine
            .search(
                "חבר".to_string(),
                vec!["/root".to_string()],
                10,
                0,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap();

        assert_eq!(results.len(), 1);
        // Snippet HTML must not contain the source <p> or </p> tags
        assert!(!results[0].text.contains("<p>"), "got: {}", results[0].text);
        assert!(!results[0].text.contains("</p>"), "got: {}", results[0].text);
    }

    #[test]
    fn test_mixed_wildcard_and_fuzzy_or_group() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלמה רב", "/books/b.txt");
        add(&mut engine, 3, "ביי חבר", "/books/c.txt");
        engine.commit().unwrap();

        // wildcard ש* OR fuzzy שלמה~1 — both are valid alternatives in the same OR slot
        let ids = search_ids(&mut engine, "שלום | שלמה~1");
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));
    }
}
