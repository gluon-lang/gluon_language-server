use gluon::base::pos::{self, BytePos, Line, Span};

use codespan::{self, ByteOffset, ColumnIndex, RawIndex, RawOffset};

use languageserver_types::{Position, Range};

use rpc::ServerError;

pub fn location_to_position(line: &str, loc: &pos::Location) -> Result<Position, ServerError<()>> {
    let column = loc.column.to_usize();
    if column <= line.len() && line.is_char_boundary(column) {
        let character = line[..column].encode_utf16().count() as u64;
        Ok(Position {
            line: loc.line.to_usize() as u64,
            character,
        })
    } else {
        Err(ServerError::from(format!(
            "{} is not a valid location",
            loc
        )))
    }
}

pub fn byte_pos_to_position<S>(
    source: &codespan::FileMap<S>,
    pos: BytePos,
) -> Result<Position, ServerError<()>>
where
    S: AsRef<str>,
{
    let (line, location) = source
        .find_line(pos)
        .ok()
        .map(|line| {
            let line_span = source.line_span(line).unwrap();
            let line_src = source.src_slice(line_span).unwrap();
            (
                line_src,
                pos::Location {
                    line,
                    column: ColumnIndex::from((pos - line_span.start()).0 as RawIndex),
                    absolute: 0.into(),
                },
            )
        })
        .ok_or_else(|| {
            error!(
                "byte_pos_to_position: {}:[{}] {}",
                source.name(),
                source.span(),
                pos
            );
            panic!("Unable to translate index to location")
        })?;
    location_to_position(line, &location)
}

pub fn byte_span_to_range<S>(
    source: &codespan::FileMap<S>,
    span: Span<BytePos>,
) -> Result<Range, ServerError<()>>
where
    S: AsRef<str>,
{
    Ok(Range {
        start: byte_pos_to_position(source, span.start())?,
        end: byte_pos_to_position(source, span.end())?,
    })
}

pub fn character_to_line_offset(line: &str, character: u64) -> Option<ByteOffset> {
    let mut character_offset = 0;
    let mut found = None;

    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if character_offset == character {
            found = Some(line.len() - chars.as_str().len() - c.len_utf8());
            break;
        }
        character_offset += c.len_utf16() as u64;
    }

    found
        .or_else(|| {
            // Handle positions after the last character on the line
            if character_offset == character {
                Some(line.len())
            } else {
                None
            }
        })
        .map(|i| ByteOffset::from(i as RawOffset))
}

pub fn position_to_byte_pos<S>(
    source: &codespan::FileMap<S>,
    position: &Position,
) -> Result<BytePos, ServerError<()>>
where
    S: AsRef<str>,
{
    source
        .line_span(Line::from(position.line as RawIndex))
        .ok()
        .and_then(|line_span| {
            character_to_line_offset(source.src_slice(line_span).unwrap(), position.character)
                .map(|byte_offset| line_span.start() + byte_offset)
        })
        .ok_or_else(|| ServerError {
            message: format!(
                "Position ({}, {}) is out of range",
                position.line, position.character
            ),
            data: None,
        })
}

pub fn range_to_byte_span<S>(
    source: &codespan::FileMap<S>,
    range: &Range,
) -> Result<Span<BytePos>, ServerError<()>>
where
    S: AsRef<str>,
{
    Ok(Span::new(
        position_to_byte_pos(source, &range.start)?,
        position_to_byte_pos(source, &range.end)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position() {
        let text = r#"
let test = 2
let test1 = ""
te
"#;
        let source = codespan::FileMap::new("".into(), text);
        let pos = position_to_byte_pos(
            &source,
            &Position {
                line: 3,
                character: 2,
            },
        ).unwrap();
        assert_eq!((3.into(), 2.into()), source.location(pos).unwrap());
    }

    // The protocol specifies that each `character` in position is a UTF-16 character.
    // This means that `å` and `ä` here counts as 1 while `𐐀` counts as 2.
    const UNICODE: &str = "åä t𐐀b";

    #[test]
    fn unicode_get_byte_pos() {
        let source = codespan::FileMap::new("".into(), UNICODE);

        let result = position_to_byte_pos(
            &source,
            &Position {
                line: 0,
                character: 3,
            },
        );
        assert_eq!(result, Ok(BytePos::from(6)));

        let result = position_to_byte_pos(
            &source,
            &Position {
                line: 0,
                character: 6,
            },
        );
        assert_eq!(result, Ok(BytePos::from(11)));
    }

    #[test]
    fn unicode_get_position() {
        let source = codespan::FileMap::new("".into(), UNICODE);

        let result = byte_pos_to_position(&source, BytePos::from(6));
        assert_eq!(
            result,
            Ok(Position {
                line: 0,
                character: 3,
            },)
        );

        let result = byte_pos_to_position(&source, BytePos::from(11));
        assert_eq!(
            result,
            Ok(Position {
                line: 0,
                character: 6,
            },)
        );
    }
}
