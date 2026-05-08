use serde::Serialize;
use std::path::PathBuf;

/// Stable in-memory identifier for a definition.
/// Maps to a position in the definitions vector.
/// Consumers should treat it as opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct DefId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Location {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SymbolKind {
    Function,
    Method,
    Constructor,
    Destructor,
    Macro,
    Variable,
    Class,
    Struct,
    Enum,
    EnumVariant,
    Typedef,
    Namespace,
    Field,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Constructor => "constructor",
            SymbolKind::Destructor => "destructor",
            SymbolKind::Macro => "macro",
            SymbolKind::Variable => "variable",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::EnumVariant => "enum_variant",
            SymbolKind::Typedef => "typedef",
            SymbolKind::Namespace => "namespace",
            SymbolKind::Field => "field",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "function" => Some(SymbolKind::Function),
            "method" => Some(SymbolKind::Method),
            "constructor" => Some(SymbolKind::Constructor),
            "destructor" => Some(SymbolKind::Destructor),
            "macro" => Some(SymbolKind::Macro),
            "variable" => Some(SymbolKind::Variable),
            "class" => Some(SymbolKind::Class),
            "struct" => Some(SymbolKind::Struct),
            "enum" => Some(SymbolKind::Enum),
            "enum_variant" => Some(SymbolKind::EnumVariant),
            "typedef" => Some(SymbolKind::Typedef),
            "namespace" => Some(SymbolKind::Namespace),
            "field" => Some(SymbolKind::Field),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Visibility {
    Public,
    Protected,
    Private,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "public" => Some(Visibility::Public),
            "protected" => Some(Visibility::Protected),
            "private" => Some(Visibility::Private),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Definition {
    pub id: Option<i64>,
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub location: Location,
    pub parent: Option<String>,
    pub signature: Option<String>,
    pub visibility: Visibility,
    pub is_definition: bool,
    pub extra: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reference {
    pub id: Option<i64>,
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    pub def_id: Option<DefId>,
    pub context: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallEdge {
    pub caller_name: String,
    pub callee_name: String,
    pub location: Location,
}

#[derive(Debug, Clone, Serialize)]
pub struct InheritEdge {
    pub derived_name: String,
    pub base_name: String,
    pub location: Location,
    pub access: Visibility,
    pub is_virtual: bool,
}

/// Metadata about an indexed file.
#[derive(Debug, Clone, Serialize)]
pub struct IndexedFile {
    pub path: PathBuf,
    pub language: String,
    pub checksum: Option<String>,
    pub symbols_checksum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileSymbols {
    pub file: PathBuf,
    pub language: String,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    pub calls: Vec<CallEdge>,
    pub inherits: Vec<InheritEdge>,
}
