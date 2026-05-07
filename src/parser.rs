use crate::types::FileSymbols;
use std::path::Path;

pub trait Parser: Send + Sync {
    fn parse_file(&self, path: &Path, source: &str) -> Result<FileSymbols, String>;
    #[allow(dead_code)]
    fn language(&self) -> &'static str;
}
