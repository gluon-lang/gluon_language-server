use std::collections::VecDeque;

use codespan::{self, RawIndex};

use languageserver_types::TextDocumentContentChangeEvent;

use gluon::base::pos::Span;

use codespan_lsp::range_to_byte_span;
use rpc::ServerError;

#[derive(Debug)]
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
        let i = self
            .changes
            .iter()
            .position(|change| change.version >= version)
            .unwrap_or(self.changes.len());
        // The client may send an empty content change event with the same version as another event
        match self.changes.get(i).map(|change| change.version) {
            Some(found_version) if found_version == version => {
                assert!(content_changes.is_empty());
            }
            _ => {}
        }
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
                change.version >= version,
                "BUG: Attempt to apply old change on newer contents {} << {:?}",
                version,
                change,
            );
            if change.version > version + 1 {
                self.changes.push_front(change);
                break;
            }
            version = change.version;
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
        apply_change(source, change)?;
    }
    Ok(())
}

// Copied from ::std::string::String::replace_range
fn replace_range(self_: &mut String, range: ::std::ops::Range<usize>, replace_with: &str) {
    // Memory safety
    //
    // Replace_range does not have the memory safety issues of a vector Splice.
    // of the vector version. The data is just plain bytes.

    assert!(self_.is_char_boundary(range.start));
    assert!(self_.is_char_boundary(range.end));

    unsafe { self_.as_mut_vec() }.splice(range, replace_with.bytes());
}

fn apply_change(
    source: &mut String,
    change: &TextDocumentContentChangeEvent,
) -> Result<(), ServerError<()>> {
    info!("Applying change: {:?}", change);
    let span = match (change.range, change.range_length) {
        (None, None) => Span::new(1.into(), (source.len() as RawIndex + 1).into()),
        (Some(range), None) | (Some(range), Some(_)) => {
            range_to_byte_span(&codespan::FileMap::new("".into(), &**source), &range)?
        }
        (None, Some(_)) => panic!("Invalid change"),
    };
    replace_range(
        source,
        (span.start().to_usize() - 1)..(span.end().to_usize() - 1),
        &change.text,
    );
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
            &[TextDocumentContentChangeEvent {
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
            }],
        ).unwrap();

        assert_eq!(source, "test");

        apply_changes(
            &mut source,
            &[TextDocumentContentChangeEvent {
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
            }],
        ).unwrap();

        assert_eq!(source, "tet");

        apply_changes(
            &mut source,
            &[TextDocumentContentChangeEvent {
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
            }],
        ).unwrap();

        assert_eq!(source, "teab");
    }
}
