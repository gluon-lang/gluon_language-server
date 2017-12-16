use gluon::base::pos::{self, BytePos, Line, Span};
use gluon::base::source;

use languageserver_types::{Position, Range};

use rpc::ServerError;

pub fn location_to_position(line: &str, loc: &pos::Location) -> Result<Position, ServerError<()>> {
    let mut next_character = 0;
    let found = line.char_indices()
        .enumerate()
        .inspect(|&(character, _)| next_character = character + 1)
        .find(|&(_, (i, _))| i == loc.column.to_usize())
        .map(|(i, _)| i as u64);

    let character = found
        .or_else(|| {
            if line.len() == loc.column.to_usize() {
                Some(next_character as u64)
            } else {
                None
            }
        })
        .ok_or_else(|| ServerError::from(format!("{} is not a valid location", loc)))?;

    Ok(Position {
        line: loc.line.to_usize() as u64,
        character,
    })
}
pub fn span_to_range(
    source: &source::Source,
    span: &Span<pos::Location>,
) -> Result<Range, ServerError<()>> {
    let (start_line, end_line) = source
        .line(Line::from(span.start.line))
        .and_then(|(_, start_line)| {
            source
                .line(Line::from(span.end.line))
                .map(|(_, end_line)| (start_line, end_line))
        })
        .ok_or_else(|| {
            ServerError::from(format!("{}:{} is not a valid span", span.start, span.end))
        })?;
    Ok(Range {
        start: location_to_position(start_line, &span.start)?,
        end: location_to_position(end_line, &span.end)?,
    })
}

pub fn byte_pos_to_location(
    source: &source::Source,
    pos: BytePos,
) -> Result<pos::Location, ServerError<()>> {
    Ok(source
        .location(pos)
        .ok_or_else(|| ServerError::from(&"Unable to translate index to location"))?)
}

pub fn byte_pos_to_position(
    source: &source::Source,
    pos: BytePos,
) -> Result<Position, ServerError<()>> {
    let (line, location) = source
        .line_at_byte(pos)
        .and_then(|(_, line)| source.location(pos).map(|location| (line, location)))
        .ok_or_else(|| ServerError::from(&"Unable to translate index to location"))?;
    location_to_position(line, &location)
}

pub fn byte_span_to_range(
    source: &source::Source,
    span: Span<BytePos>,
) -> Result<Range, ServerError<()>> {
    Ok(Range {
        start: byte_pos_to_position(source, span.start)?,
        end: byte_pos_to_position(source, span.end)?,
    })
}

pub fn character_to_line_offset(line: &str, character: u64) -> Option<BytePos> {
    let mut next_character = 0;
    let found = line.char_indices()
        .enumerate()
        .inspect(|&(i, _)| next_character = i + 1)
        .find(|&(i, (_, _))| i as u64 == character)
        .map(|(_, (byte_offset, _))| byte_offset);

    found
        .or_else(|| {
            // Handle positions after the last character on the line
            if next_character as u64 == character {
                Some(line.len())
            } else {
                None
            }
        })
        .map(BytePos::from)
}


pub fn position_to_byte_pos(
    source: &source::Source,
    position: &Position,
) -> Result<BytePos, ServerError<()>> {
    source
        .line(Line::from(position.line as usize))
        .and_then(|(line_pos, line_str)| {
            character_to_line_offset(line_str, position.character)
                .map(|byte_offset| line_pos + byte_offset)
        })
        .ok_or_else(|| {
            ServerError {
                message: format!(
                    "Position ({}, {}) is out of range",
                    position.line,
                    position.character
                ),
                data: None,
            }
        })
}

pub fn range_to_byte_span(
    source: &source::Source,
    range: &Range,
) -> Result<Span<BytePos>, ServerError<()>> {
    Ok(Span::new(
        position_to_byte_pos(source, &range.start)?,
        position_to_byte_pos(source, &range.end)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The protocol specifies that each `character` in position is a UTF-16 character.
    // This means that `å` and `ä` here counts as 1 while `𐐀` counts as 2.
    const UNICODE: &str = "åä t𐐀b";

    #[test]
    fn unicode_get_byte_pos() {
        let source = source::Source::new(UNICODE);

        let result = position_to_byte_pos(
            &source,
            &Position {
                line: 0,
                character: 3,
            },
        );
        assert_eq!(result, Ok(BytePos::from(5)));
    }

    #[test]
    fn unicode_get_position() {
        let source = source::Source::new(UNICODE);

        let result = byte_pos_to_position(&source, BytePos::from(5));
        assert_eq!(
            result,
            Ok(Position {
                line: 0,
                character: 3,
            },)
        );
    }
}
