use super::*;

fn num(raw: &str) -> Value {
    Value::Number(Number(raw.to_owned()))
}

#[test]
fn parses_nested_documents_in_order() {
    let v = parse(r#" {"b": [1, -2.5e3, true, null], "a": {"x": "y"}} "#).unwrap();
    let Value::Object(members) = &v else { panic!() };
    assert_eq!(members[0].0, "b");
    assert_eq!(
        v.get("b").unwrap(),
        &Value::Array(vec![
            num("1"),
            num("-2500.0"),
            Value::Bool(true),
            Value::Null
        ])
    );
    assert_eq!(v.get("a").unwrap().get("x").unwrap().as_str(), Some("y"));
    assert_eq!(
        v.get("b").unwrap().as_array().unwrap()[1].as_f64(),
        Some(-2500.0)
    );
}

#[test]
fn numbers_read_back_as_python_writes_them() {
    for (text, back) in [
        ("-0", "0"),
        ("1.50", "1.5"),
        ("1E3", "1000.0"),
        ("1e400", "Infinity"),
        ("12345678901234567890123", "12345678901234567890123"),
        ("0.1e-7", "1e-08"),
    ] {
        assert_eq!(to_python(&parse(text).unwrap(), None), back, "{text}");
    }
    let n = |t: &str| match parse(t).unwrap() {
        Value::Number(n) => n,
        _ => unreachable!(),
    };
    assert!(n("48000").equals(&n("48000.0")));
    assert!(!n("9007199254740993").equals(&n("9007199254740992.0")));
    assert!(!n("1").equals(&n("1.5")));
}

#[test]
fn decimals_print_and_compare_like_python_decimal() {
    for (text, shown) in [
        ("1.50", "1.50"),
        ("0.05", "0.05"),
        ("1e3", "1E+3"),
        ("-0.0", "-0.0"),
        ("1.0", "1.0"),
        ("0.0000001", "1E-7"),
        ("123.456e2", "12345.6"),
        ("0", "0"),
        ("-12", "-12"),
    ] {
        assert_eq!(decimal_text(text), shown, "{text}");
    }
    let n = |t: &str| match parse_strict(t).unwrap() {
        Value::Number(n) => n,
        _ => unreachable!(),
    };
    assert!(
        n("1.50").equals(&n("1.5")) && n("0").equals(&n("-0.0")) && n("1e3").equals(&n("1000"))
    );
    assert!(!n("0.1000000000000000055511151231257827").equals(&n("0.1")));
    assert!(
        parse_strict(r#"{"a": 1, "a": 2}"#).is_err()
            && parse_strict("NaN").is_err()
            && parse_strict("1e400").is_err()
    );
    assert_eq!(
        parse_strict("[1.50]").unwrap(),
        Value::Array(vec![Value::Number(Number("1.50".into()))])
    );
}

#[test]
fn observation_exponents_remain_exact_at_the_integer_limits() {
    let number = |text| match parse_observations(text).unwrap() {
        Value::Number(number) => number,
        _ => unreachable!(),
    };
    let tiny = "1.0e-9223372036854775808";
    assert!(!number(tiny).equals(&number("1.0")));
    assert!(!number(tiny).equals(&number("0.0")));
    assert!(number(tiny).equals(&number("1e-9223372036854775808")));
    assert!(number("100e9223372036854775806").equals(&number("10e9223372036854775807")));
    assert_eq!(observations(&parse_observations(tiny).unwrap()), tiny);
    assert_eq!(decimal_text(tiny), "1.0E-9223372036854775808");
    assert_eq!(
        decimal_text("10e9223372036854775807"),
        "1.0E+9223372036854775808"
    );
}

#[test]
fn repeated_keys_resolve_to_the_last() {
    assert_eq!(
        parse(r#"{"a": 1, "a": 2}"#).unwrap().get("a"),
        Some(&num("2"))
    );
}

#[test]
fn decodes_escapes_and_surrogate_pairs() {
    let v = parse(r#""a\"\\\/\b\f\n\r\t\u00e9\ud83d\ude00""#).unwrap();
    assert_eq!(v.as_str(), Some("a\"\\/\u{8}\u{c}\n\r\t\u{e9}\u{1f600}"));
}

#[test]
fn accepts_python_special_numbers() {
    assert!(parse("NaN").unwrap().as_f64().unwrap().is_nan());
    assert_eq!(
        parse("-Infinity").unwrap().as_f64(),
        Some(f64::NEG_INFINITY)
    );
}

#[test]
fn rejects_malformed_documents() {
    for text in [
        "",
        "{",
        "[1,]",
        "{\"a\" 1}",
        "{1: 2}",
        "01",
        "\"\t\"",
        "\"\\x\"",
        "[1] 2",
        "tru",
        "-",
        "\"\\ud800\"",
    ] {
        assert!(parse(text).is_err(), "{text:?}");
    }
    assert_eq!(
        parse("[1,\n x]").unwrap_err(),
        Error {
            message: "Expecting value".into(),
            line: 2,
            column: 2,
            offset: 5
        }
    );
}

#[test]
fn bounds_nesting_depth() {
    assert!(parse(&"[".repeat(MAX_DEPTH + 1)).is_err());
    let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
    assert!(parse(&ok).is_ok());
}

#[test]
fn truthiness_follows_python() {
    for (text, truth) in [
        ("0", false),
        ("0.0", false),
        ("1", true),
        ("\"\"", false),
        ("[]", false),
        ("{}", false),
        ("null", false),
        ("true", true),
    ] {
        assert_eq!(parse(text).unwrap().truthy(), truth, "{text}");
    }
}

#[test]
fn canonical_form_sorts_keys_compactly() {
    let v = parse(r#"{"b": [1, {"d": 2, "c": "\u00e9"}], "a": null}"#).unwrap();
    assert_eq!(canonical(&v), r#"{"a":null,"b":[1,{"c":"\u00e9","d":2}]}"#);
}

#[test]
fn writes_floats_like_python_repr() {
    for (value, text) in [
        (1.2, "1.2"),
        (1.0, "1.0"),
        (0.95, "0.95"),
        (0.8, "0.8"),
        (1e-5, "1e-05"),
        (0.0001, "0.0001"),
        (1e16, "1e+16"),
        (1.5e16, "1.5e+16"),
        (123456789012345.0, "123456789012345.0"),
        (-0.5, "-0.5"),
        (0.0, "0.0"),
        (2.5e-300, "2.5e-300"),
        // python3 -c "print(repr(17 * 2.0 ** -23))": a tie between two 17-digit strings.
        (17.0 * 2f64.powi(-23), "2.0265579223632812e-06"),
        (f64::INFINITY, "Infinity"),
    ] {
        assert_eq!(python_float(value), text);
    }
}

#[test]
fn writes_documents_like_python_json_dumps() {
    let v = Value::object([
        ("a", Value::int(1)),
        ("b", Value::Array(vec![])),
        ("c", Value::object(Vec::<(String, Value)>::new())),
        ("d", Value::strings(["x\u{e9}\u{1f600}\"\n\u{1}"])),
        (
            "e",
            Value::Array(vec![Value::Null, Value::Bool(true), Value::float(0.5)]),
        ),
    ]);
    assert_eq!(
        to_python(&v, Some(2)),
        "{\n  \"a\": 1,\n  \"b\": [],\n  \"c\": {},\n  \"d\": [\n    \"x\\u00e9\\ud83d\\ude00\\\"\\n\\u0001\"\n  ],\n  \"e\": [\n    null,\n    true,\n    0.5\n  ]\n}"
    );
    assert_eq!(
        to_python(&v, None),
        "{\"a\": 1, \"b\": [], \"c\": {}, \"d\": [\"x\\u00e9\\ud83d\\ude00\\\"\\n\\u0001\"], \"e\": [null, true, 0.5]}"
    );
    assert_eq!(parse(&to_python(&v, Some(2))).unwrap(), v);
}
