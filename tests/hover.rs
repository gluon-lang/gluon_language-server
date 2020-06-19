mod support;

use languageserver_types::*;

use crate::support::{expect_notification, expect_response, hover};

const STREAM_SOURCE: &'static str = r#"
let prelude = import! "std/prelude.glu"
let { Num } = prelude
let { Option } = import! "std/option.glu"

rec
type Stream_ a =
    | Value a (Stream a)
    | Empty
type Stream a = Lazy (Stream_ a)
in

let from f : (Int -> Option a) -> Stream a =
        let from_ i =
                lazy (\_ ->
                    match f i with
                    | Some x -> Value x (from_ (i + 1))
                    | None -> Empty
                )
        in from_ 0

{ from }
"#;

#[test]
fn simple_hover() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let uri = "file:///c%3A/test/test.glu";
            support::did_open(stdin, uri, "123").await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            hover(
                stdin,
                2,
                uri,
                Position {
                    line: 0,
                    character: 2,
                },
            )
            .await;
            let hover: Hover = expect_response(stdout).await;

            assert_eq!(
                hover,
                Hover {
                    contents: HoverContents::Scalar(MarkedString::String("Int".into())),
                    range: Some(Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 3,
                        },
                    }),
                }
            );
        })
    });
}

#[test]
fn identifier() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            let src = r#"
let test = 1
test
"#;
            support::did_open(stdin, "test", src).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            hover(
                stdin,
                2,
                "test",
                Position {
                    line: 2,
                    character: 2,
                },
            )
            .await;

            let hover: Hover = expect_response(stdout).await;

            assert_eq!(
                hover,
                Hover {
                    contents: HoverContents::Scalar(MarkedString::String("Int".into())),
                    range: Some(Range {
                        start: Position {
                            line: 2,
                            character: 0,
                        },
                        end: Position {
                            line: 2,
                            character: 4,
                        },
                    }),
                }
            );
        })
    });
}

#[test]
fn stream() {
    support::send_rpc(move |stdin, stdout| {
        Box::pin(async move {
            support::did_open(stdin, "stream", STREAM_SOURCE).await;

            let _: PublishDiagnosticsParams = expect_notification(&mut *stdout).await;

            hover(
                stdin,
                2,
                "stream",
                Position {
                    line: 15,
                    character: 29,
                },
            )
            .await;

            let hover: Option<Hover> = expect_response(stdout).await;

            assert_eq!(
                hover,
                Some(Hover {
                    contents: HoverContents::Scalar(MarkedString::String("Int".into())),
                    range: Some(Range {
                        start: Position {
                            line: 15,
                            character: 28,
                        },
                        end: Position {
                            line: 15,
                            character: 29,
                        },
                    }),
                })
            );
        })
    });
}
