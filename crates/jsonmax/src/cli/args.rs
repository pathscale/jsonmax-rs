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
    let mut native: Option<PathBuf> = None;

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
            "--native" => (OptionKind::Native, false),
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
            OptionKind::Native => {
                if native.is_some() {
                    return Err(usage("native specification specified more than once"));
                }
                native = Some(PathBuf::from(value));
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
    if native.is_some() {
        if !formats.contains(&OutputFormat::Rust) {
            return Err(usage("--native needs the rust format"));
        }
        if legacy {
            return Err(usage("--native cannot be combined with legacy options"));
        }
    }

    Ok(Command::Generate(Options {
        input,
        output,
        formats,
        mode: if check { RunMode::Check } else { RunMode::Write },
        legacy,
        native,
    }))
}

#[derive(Clone, Copy)]
enum OptionKind {
    Input,
    Output,
    Format,
    Native,
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
            | "--native"
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
