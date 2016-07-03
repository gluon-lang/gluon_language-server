use serde;
use serde_json::Value;

#[derive(Deserialize)]
pub struct DidChangeTextDocumentParams {
    #[serde(rename="textDocument")]
    pub text_document: VersionedTextDocumentIdentifier,
    #[serde(rename="contentChanges")]
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Deserialize)]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

#[derive(Deserialize)]
pub struct VersionedTextDocumentIdentifier {
    pub version: u64,
    pub uri: String,
}

#[derive(Deserialize)]
pub struct TextDocumentContentChangeEvent {
    pub range: Option<Range>,
    #[serde(rename="rangeLength")]
    pub range_length: Option<u64>,
    pub text: String,
}

#[derive(Default, Deserialize, Serialize)]
pub struct Position {
    pub line: u64,
    pub character: u64,
}

#[derive(Default, Deserialize, Serialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Deserialize)]
pub struct TextDocumentPositionParams {
    #[serde(rename="textDocument")]
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

#[derive(Deserialize)]
pub struct InitializeParams {
    /**
     * The process Id of the parent process that started
     * the server.
     */
    #[serde(rename="processId")]
    pub process_id: u64,

    /**
     * The rootPath of the workspace. Is null
     * if no folder is open.
     */
    #[serde(rename="rootPath")]
    pub root_path: Option<String>,

    /**
     * The capabilities provided by the client (editor)
     */
    pub capabilities: ClientCapabilities,
}

#[derive(Deserialize)]
pub struct ClientCapabilities { _dummy: Option<()> }

#[derive(Default, Serialize)]
pub struct InitializeResult {
    pub capabilities: ServerCapabilities,
}

#[derive(Default, Serialize)]
pub struct InitializeError {
    /**
     * Indicates whether the client should retry to send the
     * initilize request after showing the message provided
     * in the ResponseError.
     */
    pub retry: bool,
}

#[derive(Default, Serialize)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if="Option::is_none")]
    #[serde(rename="textDocumentSync")]
    pub text_document_sync: Option<TextDocumentSyncKind>,
    #[serde(skip_serializing_if="Option::is_none")]
    #[serde(rename="completionProvider")]
    pub completion_provider: Option<CompletionOptions>,
}

#[derive(Clone, Copy)]
pub enum TextDocumentSyncKind {
    None = 0,
    Full = 1,
    Incremental = 2,
}

impl serde::Serialize for TextDocumentSyncKind {
    fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
        where S: serde::Serializer
    {
        serializer.serialize_u8(*self as u8)
    }
}


#[derive(Default, Serialize)]
pub struct CompletionOptions {
    #[serde(skip_serializing_if="Option::is_none")]
    #[serde(rename="resolveProvider")]
    pub resolve_provider: Option<bool>,
    #[serde(rename="triggerCharacters")]
    pub trigger_characters: Vec<String>,
}

#[derive(Default, Serialize)]
pub struct TextEdit {
    pub range: Range,
    #[serde(rename="newText")]
    pub new_text: String,
}

/**
 * Represents a collection of [completion items](#CompletionItem) to be presented
 * in the editor.
 */
#[derive(Default, Serialize)]
pub struct CompletionList {
    /**
     * This list it not complete. Further typing should result in recomputing
     * this list.
     */
    #[serde(rename="isIncomplete")]
    pub is_incomplete: bool,
    /**
     * The completion items.
     */
    pub items: Vec<CompletionItem>,
}

#[derive(Default, Serialize)]
pub struct CompletionItem {
    /**
     * The label of this completion item. By default
     * also the text that is inserted when selecting
     * this completion.
     */
    pub label: String,
    /**
     * The kind of this completion item. Based of the kind
     * an icon is chosen by the editor.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    pub kind: Option<CompletionItemKind>,
    /**
     * A human-readable string with additional information
     * about this item, like type or symbol information.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    pub detail: Option<String>,
    /**
     * A human-readable string that represents a doc-comment.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    pub documentation: Option<String>,
    /**
     * A string that shoud be used when comparing this item
     * with other items. When `falsy` the label is used.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    #[serde(rename="sortText")]
    pub sort_text: Option<String>,
    /**
     * A string that should be used when filtering a set of
     * completion items. When `falsy` the label is used.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    #[serde(rename="filterText")]
    pub filter_text: Option<String>,
    /**
     * A string that should be inserted a document when selecting
     * this completion. When `falsy` the label is used.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    #[serde(rename="insertText")]
    pub insert_text: Option<String>,
    /**
     * An edit which is applied to a document when selecting
     * this completion. When an edit is provided the value of
     * insertText is ignored.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    #[serde(rename="textEdit")]
    pub text_edit: Option<TextEdit>,
    /**
     * An data entry field that is preserved on a completion item between
     * a completion and a completion resolve request.
     */
    #[serde(skip_serializing_if="Option::is_none")]
    pub data: Option<Value>,
}

/**
 * The kind of a completion entry.
 */
#[derive(Clone, Copy)]
pub enum CompletionItemKind {
    Text = 1,
    Method = 2,
    Function = 3,
    Constructor = 4,
    Field = 5,
    Variable = 6,
    Class = 7,
    Interface = 8,
    Module = 9,
    Property = 10,
    Unit = 11,
    Value = 12,
    Enum = 13,
    Keyword = 14,
    Snippet = 15,
    Color = 16,
    File = 17,
    Reference = 18,
}

impl serde::Serialize for CompletionItemKind {
    fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
        where S: serde::Serializer
    {
        serializer.serialize_u8(*self as u8)
    }
}

#[derive(Serialize)]
pub struct ShowMessageParams {
    /**
     * The message type. See {@link MessageType}
     */
    #[serde(rename="type")]
    pub typ: MessageType,

    /**
     * The actual message
     */
    pub message: String,
}

#[derive(Serialize)]
pub struct LogMessageParams {
    /**
     * The message type. See {@link MessageType}
     */
    #[serde(rename="type")]
    pub typ: MessageType,

    /**
     * The actual message
     */
    pub message: String,
}

#[derive(Clone, Copy)]
pub enum MessageType {
    /**
     * An error message.
     */
    Error = 1,
    /**
     * A warning message.
     */
    Warning = 2,
    /**
     * An information message.
     */
    Info = 3,
    /**
     * A log message.
     */
    Log = 4,
}

impl serde::Serialize for MessageType {
    fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
        where S: serde::Serializer
    {
        serializer.serialize_u8(*self as u8)
    }
}

#[derive(Default, Serialize)]
pub struct PublishDiagnosticsParams {
    /**
     * The URI for which diagnostic information is reported.
     */
    pub uri: String,

    /**
     * An array of diagnostic information items.
     */
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Default, Serialize)]
pub struct Diagnostic {
    /**
     * The range at which the message applies
     */
    pub range: Range,

    /**
     * The diagnostic's severity. Can be omitted. If omitted it is up to the
     * client to interpret diagnostics as error, warning, info or hint.
     */
    pub severity: Option<DiagnosticSeverity>,

    /**
     * The diagnostic's code. Can be omitted.
     */
    pub code: String, // number | string;

    /**
     * A human-readable string describing the source of this
     * diagnostic, e.g. 'typescript' or 'super lint'.
     */
    pub source: Option<String>,

    /**
     * The diagnostic's message.
     */
    pub message: String,
}

#[derive(Clone, Copy)]
pub enum DiagnosticSeverity {
    /**
     * Reports an error.
     */
    Error = 1,
    /**
     * Reports a warning.
     */
    Warning = 2,
    /**
     * Reports an information.
     */
    Information = 3,
    /**
     * Reports a hint.
     */
    Hint = 4,
}

impl serde::Serialize for DiagnosticSeverity {
    fn serialize<S>(&self, serializer: &mut S) -> Result<(), S::Error>
        where S: serde::Serializer
    {
        serializer.serialize_u8(*self as u8)
    }
}
