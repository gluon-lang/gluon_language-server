use std::collections::VecDeque;

use languageserver_types::TextDocumentContentChangeEvent;

use gluon::base::source;
use gluon::base::pos::Span;

use rpc::ServerError;
use range_to_byte_span;

struct VersionedChange {
    version: u64,
    content_changes: Vec<TextDocumentContentChangeEvent>,
}


/// Type which applies text changes in the order that the client sends them.
/// Out of order changes are stored until all earlier changes have been received after which they
/// are applied all at once.
pub struct TextChanges {
    changes: VecDeque<VersionedChange>,
}

impl TextChanges {
    pub fn new() -> Self {
        TextChanges {
            changes: VecDeque::new(),
        }
    }

    pub fn add(&mut self, version: u64, content_changes: Vec<TextDocumentContentChangeEvent>) {
        let i = self.changes
            .iter()
            .position(|change| change.version > version)
            .unwrap_or(self.changes.len());
        self.changes.insert(
            i,
            VersionedChange {
                version,
                content_changes,
            },
        );
    }

    pub fn apply_changes(
        &mut self,
        source: &mut String,
        mut version: u64,
    ) -> Result<u64, ServerError<()>> {
        while let Some(change) = self.changes.pop_front() {
            assert!(
                (change.version == version && change.content_changes.is_empty())
                    || change.version > version,
                "BUG: Attempt to apply old change on newer contents"
            );
            if change.content_changes.is_empty() {
                continue;
            }
            if version + 1 != change.version {
                self.changes.push_front(change);
                break;
            }
            version += 1;
            apply_changes(source, &change.content_changes)?
        }
        Ok(version)
    }
}

fn apply_changes(
    source: &mut String,
    content_changes: &[TextDocumentContentChangeEvent],
) -> Result<(), ServerError<()>> {
    for change in content_changes {
        let lines = source::Lines::new(source.bytes());
        apply_change(source, lines, change)?;
    }
    Ok(())
}

fn apply_change(
    source: &mut String,
    lines: source::Lines,
    change: &TextDocumentContentChangeEvent,
) -> Result<(), ServerError<()>> {
    info!("Applying change: {:?}", change);
    let span = match (change.range, change.range_length) {
        (None, None) => Span::new(0.into(), source.len().into()),
        (Some(range), None) | (Some(range), Some(_)) => {
            range_to_byte_span(&source::Source::with_lines(source, lines), &range)?
        }
        (None, Some(_)) => panic!("Invalid change"),
    };
    if let Some(range_length) = change.range_length {
        assert!(range_length as usize == (span.end - span.start).to_usize());
    }
    source.drain(span.start.to_usize()..span.end.to_usize());
    source.insert_str(span.start.to_usize(), &change.text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use languageserver_types::{Position, Range};


    #[test]
    fn apply_changes_test() {
        let mut source = String::new();
        apply_changes(
            &mut source,
            &[
                TextDocumentContentChangeEvent {
                    range: Some(Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    }),
                    range_length: None,
                    text: "test".to_string(),
                },
            ],
        ).unwrap();

        assert_eq!(source, "test");

        apply_changes(
            &mut source,
            &[
                TextDocumentContentChangeEvent {
                    range: Some(Range {
                        start: Position {
                            line: 0,
                            character: 2,
                        },
                        end: Position {
                            line: 0,
                            character: 3,
                        },
                    }),
                    range_length: Some(1),
                    text: "".to_string(),
                },
            ],
        ).unwrap();

        assert_eq!(source, "tet");

        apply_changes(
            &mut source,
            &[
                TextDocumentContentChangeEvent {
                    range: Some(Range {
                        start: Position {
                            line: 0,
                            character: 2,
                        },
                        end: Position {
                            line: 0,
                            character: 3,
                        },
                    }),
                    range_length: Some(1),
                    text: "ab".to_string(),
                },
            ],
        ).unwrap();

        assert_eq!(source, "teab");
    }
}
