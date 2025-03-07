// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use mz_sqllogictest::ast::{Location, Record};
use mz_sqllogictest::parser;

#[mz_ore::test]
fn test_parser() {
    struct TestCase {
        input: &'static str,
        output: Vec<Record<'static>>,
    }

    fn linenum(n: usize) -> Location {
        Location {
            file: "<test>".to_string(),
            line: n,
        }
    }

    let test_cases = vec![
        TestCase {
            input: "statement ok
SELECT 1",
            output: vec![Record::Statement {
                expected_error: None,
                rows_affected: None,
                sql: "SELECT 1",
                location: linenum(2),
            }],
        },
        TestCase {
            input: "statement OK
SELECT 1",
            output: vec![Record::Statement {
                expected_error: None,
                rows_affected: None,
                sql: "SELECT 1",
                location: linenum(2),
            }],
        },
        TestCase {
            input: "statement count 7
SELECT 1",
            output: vec![Record::Statement {
                expected_error: None,
                rows_affected: Some(7),
                sql: "SELECT 1",
                location: linenum(2),
            }],
        },
        TestCase {
            input: "statement error this statement is wrong
SELECT blargh",
            output: vec![Record::Statement {
                expected_error: Some("this statement is wrong"),
                rows_affected: None,
                sql: "SELECT blargh",
                location: linenum(2),
            }],
        },
        TestCase {
            input: "halt

statement ok
SELECT disappear",
            output: vec![],
        },
        TestCase {
            input: "skipif postgresql
statement ok
SELECT not_postgresql

onlyif postgresql
statement ok
SELECT only_postgresql

statement ok
SELECT everybody

skipif bloop
skipif blorp
statement ok
SELECT multiskip_not_us

skipif bloop
skipif postgresql
skipif blorp
statement ok
SELECT multiskip_including_us

onlyif postgresql
halt

statement ok
SELECT disappear",
            output: vec![
                Record::Statement {
                    expected_error: None,
                    rows_affected: None,
                    sql: "SELECT only_postgresql",
                    location: linenum(7),
                },
                Record::Statement {
                    expected_error: None,
                    rows_affected: None,
                    sql: "SELECT everybody",
                    location: linenum(10),
                },
                Record::Statement {
                    expected_error: None,
                    rows_affected: None,
                    sql: "SELECT multiskip_not_us",
                    location: linenum(15),
                },
            ],
        },
    ];

    for tc in test_cases {
        let mut parser = crate::parser::Parser::new("<test>", tc.input);
        let records = parser.parse_records().unwrap();
        assert_eq!(records, tc.output);
    }
}
