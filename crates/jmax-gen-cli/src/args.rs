//! Native and version-1 legacy command-line option parsing.

use super::{CliError, CliResult, Command, Options, OutputFormat, RunMode};
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

/// Parse one format name. The returned formats always follow declaration order.
pub fn parse_format(value: &str) -> CliResult<Vec<OutputFormat>> {
    let format = match value {
        "rust" => OutputFormat::Rust,
        "services" => OutputFormat::Services,
        "json-schema" => OutputFormat::JsonSchema,
        "openapi" => OutputFormat::OpenApi,
        "asyncapi" => OutputFormat::AsyncApi,
        "markdown" => OutputFormat::Markdown,
        "all" => return Ok(all_formats()),
        "" => return Err(usage("format must not be empty")),
        _ => return Err(usage(format!("unknown format: {value}"))),
    };
    Ok(vec![format])
}

/// Parse arguments excluding argv[0]. No filesystem access is performed here.
pub fn parse_args<I>(arguments: I) -> CliResult<Command>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = arguments.into_iter();
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut formats: Vec<OutputFormat> = Vec::new();
    let mut saw_format = false;
    let mut check = false;
    let mut legacy = false;

    while let Some(argument) = args.next() {
        let Some(argument_text) = argument.to_str() else {
            if looks_like_option(&argument) {
                return Err(usage("option names must be valid UTF-8"));
            }
            return Err(usage("unexpected positional argument"));
        };

        // Help and version intentionally short-circuit even if later arguments
        // would otherwise be invalid or required values are absent.
        if argument_text == "--help" || argument_text == "-h" {
            return Ok(Command::Help);
        }
        if argument_text == "--version" || argument_text == "-V" {
            return Ok(Command::Version);
        }

        if argument_text == "--check" {
            if check {
                return Err(usage("duplicate --check mode flag"));
            }
            check = true;
            continue;
        }
        if argument_text == "--legacy" {
            legacy = true;
            continue;
        }

        if matches!(argument_text, "--rust" | "--docs" | "--spec") {
            legacy = true;
            let format = legacy_format(argument_text)
                .ok_or_else(|| usage(format!("unknown option: {argument_text}")))?;
            saw_format = true;
            formats.push(format);
            continue;
        }

        let (name, equals_value) = match argument_text.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (argument_text, None),
        };

        let (kind, alias) = match name {
            "--input" | "-i" => (OptionKind::Input, false),
            "--output" | "-o" => (OptionKind::Output, false),
            "--format" | "-f" => (OptionKind::Format, false),
            "--input-dir" => (OptionKind::Input, true),
            "--output-dir" => (OptionKind::Output, true),
            _ => return Err(usage(format!("unknown option: {argument_text}"))),
        };

        let value = match equals_value {
            Some(value) => OsString::from(value),
            None => {
                let Some(value) = args.next() else {
                    return Err(usage(format!("missing value for {name}")));
                };
                if value == OsStr::new("--help") || value == OsStr::new("-h") {
                    return Ok(Command::Help);
                }
                if value == OsStr::new("--version") || value == OsStr::new("-V") {
                    return Ok(Command::Version);
                }
                if is_recognized_flag(&value) {
                    return Err(usage(format!("missing value for {name}")));
                }
                value
            }
        };

        if value.is_empty() {
            return Err(usage(format!("value for {name} must not be empty")));
        }

        match kind {
            OptionKind::Input => {
                if input.is_some() {
                    return Err(usage("input path specified more than once"));
                }
                input = Some(PathBuf::from(value));
            }
            OptionKind::Output => {
                if output.is_some() {
                    return Err(usage("output directory specified more than once"));
                }
                output = Some(PathBuf::from(value));
            }
            OptionKind::Format => {
                let Some(text) = value.to_str() else {
                    return Err(usage("format names must be valid UTF-8"));
                };
                saw_format = true;
                formats.extend(parse_format(text)?);
            }
        }

        if alias {
            legacy = true;
        }
    }

    let Some(input) = input else {
        return Err(usage("--input is required"));
    };
    let Some(output) = output else {
        return Err(usage("--output is required"));
    };

    let formats = if saw_format {
        canonicalize_formats(formats)
    } else {
        all_formats()
    };
    if formats.is_empty() {
        return Err(usage("at least one output format is required"));
    }

    Ok(Command::Generate(Options {
        input,
        output,
        formats,
        mode: if check { RunMode::Check } else { RunMode::Write },
        legacy,
    }))
}

#[derive(Clone, Copy)]
enum OptionKind {
    Input,
    Output,
    Format,
}

fn all_formats() -> Vec<OutputFormat> {
    vec![
        OutputFormat::Rust,
        OutputFormat::Services,
        OutputFormat::JsonSchema,
        OutputFormat::OpenApi,
        OutputFormat::AsyncApi,
        OutputFormat::Markdown,
    ]
}

fn legacy_format(option: &str) -> Option<OutputFormat> {
    match option {
        "--rust" => Some(OutputFormat::Rust),
        "--docs" => Some(OutputFormat::Markdown),
        "--spec" => Some(OutputFormat::Services),
        _ => None,
    }
}

fn canonicalize_formats(mut formats: Vec<OutputFormat>) -> Vec<OutputFormat> {
    formats.sort_unstable();
    formats.dedup();
    formats
}

fn usage(message: impl Into<String>) -> CliError {
    CliError::Usage(message.into())
}

fn looks_like_option(value: &OsStr) -> bool {
    value.to_string_lossy().starts_with('-')
}

fn is_recognized_flag(value: &OsStr) -> bool {
    let Some(value) = value.to_str() else {
        return false;
    };
    matches!(
        value,
        "--input"
            | "-i"
            | "--output"
            | "-o"
            | "--format"
            | "-f"
            | "--check"
            | "--legacy"
            | "--help"
            | "-h"
            | "--version"
            | "-V"
            | "--input-dir"
            | "--output-dir"
            | "--rust"
            | "--docs"
            | "--spec"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parsed(values: &[&str]) -> Options {
        match parse_args(os(values)).expect("valid arguments") {
            Command::Generate(options) => options,
            other => panic!("expected generation command, got {other:?}"),
        }
    }

    fn assert_usage(result: CliResult<Command>) {
        assert!(matches!(result, Err(CliError::Usage(_))));
    }

    #[test]
    fn formats_are_single_names_and_canonical() {
        for (name, expected) in [
            ("rust", OutputFormat::Rust),
            ("services", OutputFormat::Services),
            ("json-schema", OutputFormat::JsonSchema),
            ("openapi", OutputFormat::OpenApi),
            ("asyncapi", OutputFormat::AsyncApi),
            ("markdown", OutputFormat::Markdown),
        ] {
            assert_eq!(parse_format(name).unwrap(), vec![expected]);
        }
        assert_eq!(
            parse_format("all").unwrap(),
            vec![
                OutputFormat::Rust,
                OutputFormat::Services,
                OutputFormat::JsonSchema,
                OutputFormat::OpenApi,
                OutputFormat::AsyncApi,
                OutputFormat::Markdown,
            ]
        );
        assert_usage(parse_format("").map(|_| Command::Help));
        assert_usage(parse_format("rust,services").map(|_| Command::Help));
        assert_usage(parse_format("yaml").map(|_| Command::Help));
    }

    #[test]
    fn defaults_and_native_options_parse() {
        let defaults = parsed(&["--input", "services.ron", "--output", "out"]);
        assert_eq!(defaults.input, PathBuf::from("services.ron"));
        assert_eq!(defaults.output, PathBuf::from("out"));
        assert_eq!(defaults.formats, all_formats());
        assert_eq!(defaults.mode, RunMode::Write);
        assert!(!defaults.legacy);

        let selected = parsed(&[
            "-i=source dir",
            "-o=generated",
            "--format=markdown",
            "-f",
            "rust",
            "--format=markdown",
            "--check",
        ]);
        assert_eq!(selected.formats, vec![OutputFormat::Rust, OutputFormat::Markdown]);
        assert_eq!(selected.mode, RunMode::Check);
    }

    #[test]
    fn format_sorting_and_all_expansion_are_stable() {
        let options = parsed(&[
            "--input=x",
            "--output=y",
            "-f",
            "markdown",
            "-f",
            "all",
            "-f",
            "rust",
        ]);
        assert_eq!(options.formats, all_formats());
    }

    #[test]
    fn legacy_aliases_set_legacy_and_select_expected_values() {
        let options = parsed(&[
            "--input-dir",
            "legacy-root",
            "--output-dir=out",
            "--rust",
            "--docs",
            "--spec",
            "--format",
            "openapi",
        ]);
        assert!(options.legacy);
        assert_eq!(options.input, PathBuf::from("legacy-root"));
        assert_eq!(options.output, PathBuf::from("out"));
        assert_eq!(
            options.formats,
            vec![
                OutputFormat::Rust,
                OutputFormat::Services,
                OutputFormat::OpenApi,
                OutputFormat::Markdown,
            ]
        );
    }

    #[test]
    fn help_and_version_short_circuit_at_encounter() {
        assert_eq!(parse_args(os(&["--help", "--unknown"])).unwrap(), Command::Help);
        assert_eq!(parse_args(os(&["--input", "--version"])).unwrap(), Command::Version);
        assert_eq!(parse_args(os(&["-h", "--input"])).unwrap(), Command::Help);
        assert_eq!(parse_args(os(&["-V"])).unwrap(), Command::Version);
    }

    #[test]
    fn missing_empty_duplicate_and_unknown_values_are_usage_errors() {
        for args in [
            vec![],
            vec!["--input"],
            vec!["--output", "out"],
            vec!["--input", "--output", "out"],
            vec!["--input", "", "--output", "out"],
            vec!["--input", "in", "--output", ""],
            vec!["--input=x", "--input=y", "--output", "out"],
            vec!["--input=x", "--output=y", "--output=z"],
            vec!["--input=x", "--output=y", "--check", "--check"],
            vec!["--input=x", "--output=y", "--format="],
            vec!["--input=x", "--output=y", "--format", ""],
            vec!["--input=x", "--output=y", "--format", "unknown"],
            vec!["--input=x", "--output=y", "stray"],
            vec!["--wat"],
            vec!["--input=x", "--output=y", "--legacy=true"],
            vec!["--input=x", "--output=y", "--rust=rust"],
        ] {
            assert_usage(parse_args(os(&args)));
        }
    }

    #[test]
    fn check_and_legacy_are_valid_without_format_flags() {
        let options = parsed(&["--input=x", "--output=y", "--legacy", "--check"]);
        assert!(options.legacy);
        assert_eq!(options.mode, RunMode::Check);
        assert_eq!(options.formats, all_formats());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_preserved_but_non_utf8_format_is_rejected() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let input = OsString::from_vec(vec![b'i', b'n', 0xff]);
        let args = vec![
            OsString::from("--input"),
            input.clone(),
            OsString::from("--output=out"),
        ];
        let Command::Generate(options) = parse_args(args).unwrap() else {
            panic!("expected generation command");
        };
        assert_eq!(options.input.as_os_str().as_bytes(), input.as_bytes());

        let result = parse_args(vec![
            OsString::from("--input=x"),
            OsString::from("--output=y"),
            OsString::from("--format"),
            OsString::from_vec(vec![0xff]),
        ]);
        assert_usage(result);
    }
}
