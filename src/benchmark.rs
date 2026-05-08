use std::path::PathBuf;
use std::time::Instant;

use serde::Serialize;

use crate::{IndexMetrics, Indexer, Language, ReferenceMode, StreamedDbWriteMetrics};

/// Configuration for a benchmarked indexing run.
#[derive(Debug, Clone)]
pub struct BenchmarkConfig {
    pub roots: Vec<PathBuf>,
    pub language: Language,
    pub follow_symlinks: bool,
    pub threads: Option<usize>,
    pub reference_mode: ReferenceMode,
    pub include_reference_context: bool,
    pub db_path: Option<PathBuf>,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            language: Language::Cpp,
            follow_symlinks: false,
            threads: None,
            reference_mode: ReferenceMode::None,
            include_reference_context: false,
            db_path: None,
        }
    }
}

/// Timing information for SQLite persistence.
#[derive(Debug, Clone, Serialize)]
pub struct SaveMetrics {
    pub db: String,
    pub save_ms: u128,
    pub writer: Option<StreamedDbWriteMetrics>,
    pub db_bytes: Option<u64>,
}

/// Full benchmark report for an index run and optional database save.
#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkReport {
    pub roots: Vec<String>,
    pub language: &'static str,
    pub follow_symlinks: bool,
    pub reference_mode: ReferenceMode,
    pub include_reference_context: bool,
    pub index: IndexMetrics,
    pub save: Option<SaveMetrics>,
    pub total_ms: u128,
}

/// Run the indexer with phase timings and optional SQLite persistence timing.
pub fn run(config: BenchmarkConfig) -> Result<BenchmarkReport, String> {
    if config.roots.is_empty() {
        return Err("benchmark requires at least one root".to_string());
    }

    let total_start = Instant::now();
    let mut builder = Indexer::builder()
        .language(config.language)
        .follow_symlinks(config.follow_symlinks)
        .reference_mode(config.reference_mode)
        .reference_context(config.include_reference_context);

    if let Some(threads) = config.threads {
        builder = builder.threads(threads);
    }
    for root in &config.roots {
        builder = builder.add_directory(root);
    }

    let (index_metrics, save) = if let Some(db_path) = &config.db_path {
        let run = builder.build().index_db(db_path).map_err(|e| {
            format!(
                "benchmark streamed index failed for '{}': {e}",
                db_path.display()
            )
        })?;
        (
            run.metrics,
            Some(SaveMetrics {
                db: db_path.to_string_lossy().to_string(),
                save_ms: run.save_ms,
                writer: Some(run.writer),
                db_bytes: std::fs::metadata(db_path).ok().map(|meta| meta.len()),
            }),
        )
    } else {
        let run = builder
            .build()
            .index_with_metrics()
            .map_err(|e| format!("benchmark index failed: {e}"))?;
        (run.metrics, None)
    };

    Ok(BenchmarkReport {
        roots: config
            .roots
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
        language: language_name(config.language),
        follow_symlinks: config.follow_symlinks,
        reference_mode: config.reference_mode,
        include_reference_context: config.include_reference_context,
        index: index_metrics,
        save,
        total_ms: total_start.elapsed().as_millis(),
    })
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::C => "c",
        Language::Cpp => "cpp",
    }
}
