use std::io::{self, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::command::OutputMode;
use crate::error::CliError;

pub struct Output {
    mode: OutputMode,
    quiet: bool,
    request_id: String,
    source: AtomicU8,
    capture: Option<Arc<Mutex<Option<CapturedOutput>>>>,
}

#[derive(Debug, Clone)]
pub struct CapturedOutput {
    pub data: Value,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
pub struct CaptureHandle(Arc<Mutex<Option<CapturedOutput>>>);

impl CaptureHandle {
    pub fn take(&self) -> Option<CapturedOutput> {
        self.0.lock().ok()?.take()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Meta<'a> {
    request_id: &'a str,
    source: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SuccessEnvelope<'a, T> {
    schema_version: &'static str,
    ok: bool,
    data: &'a T,
    warnings: &'a [String],
    meta: Meta<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    code: String,
    message: String,
    retryable: bool,
    details: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope<'a> {
    schema_version: &'static str,
    ok: bool,
    error: ErrorBody,
    warnings: &'a [String],
    meta: Meta<'a>,
}

impl Output {
    pub fn new(mode: OutputMode, quiet: bool) -> Self {
        Self::new_with_request_id(mode, quiet, None)
    }

    pub fn new_with_request_id(mode: OutputMode, quiet: bool, request_id: Option<String>) -> Self {
        Self {
            mode,
            quiet,
            request_id: request_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            source: AtomicU8::new(0),
            capture: None,
        }
    }

    pub fn capture() -> (Self, CaptureHandle) {
        let capture = Arc::new(Mutex::new(None));
        (
            Self {
                mode: OutputMode::Json,
                quiet: true,
                request_id: Uuid::new_v4().to_string(),
                source: AtomicU8::new(1),
                capture: Some(capture.clone()),
            },
            CaptureHandle(capture),
        )
    }

    pub fn mark_owner(&self) {
        self.source.store(1, Ordering::Relaxed);
    }

    fn source(&self) -> &'static str {
        if self.source.load(Ordering::Relaxed) == 1 {
            "owner"
        } else {
            "direct"
        }
    }

    pub fn success<T: Serialize>(&self, data: &T, warnings: &[String]) -> Result<(), CliError> {
        if let Some(capture) = &self.capture {
            *capture.lock().map_err(|_| {
                CliError::InvalidInput("output capture lock poisoned".to_string())
            })? = Some(CapturedOutput {
                data: serde_json::to_value(data)?,
                warnings: warnings.to_vec(),
            });
            return Ok(());
        }
        match self.mode {
            OutputMode::Human => {
                let value = serde_json::to_value(data)?;
                render_human(&value, io::stdout().lock())?;
                if !self.quiet {
                    for warning in warnings {
                        eprintln!("warning: {warning}");
                    }
                }
            }
            OutputMode::Json => {
                let envelope = SuccessEnvelope {
                    schema_version: "1",
                    ok: true,
                    data,
                    warnings,
                    meta: Meta {
                        request_id: &self.request_id,
                        source: self.source(),
                    },
                };
                serde_json::to_writer_pretty(io::stdout().lock(), &envelope)?;
                println!();
            }
            OutputMode::Jsonl => {
                let value = serde_json::to_value(data)?;
                match value {
                    Value::Array(items) => {
                        let mut out = io::stdout().lock();
                        for item in items {
                            serde_json::to_writer(&mut out, &item)?;
                            writeln!(out)?;
                        }
                    }
                    other => {
                        serde_json::to_writer(io::stdout().lock(), &other)?;
                        println!();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn error(&self, error: &CliError) {
        match self.mode {
            OutputMode::Human => eprintln!("error [{}]: {}", error.code(), error),
            OutputMode::Json | OutputMode::Jsonl => {
                let envelope = ErrorEnvelope {
                    schema_version: "1",
                    ok: false,
                    error: ErrorBody {
                        code: error.code().to_string(),
                        message: error.to_string(),
                        retryable: error.retryable(),
                        details: error.details(),
                    },
                    warnings: &[],
                    meta: Meta {
                        request_id: &self.request_id,
                        source: self.source(),
                    },
                };
                let _ = serde_json::to_writer(io::stderr().lock(), &envelope);
                eprintln!();
            }
        }
    }
}

fn render_human(mut value: &Value, mut out: impl Write) -> io::Result<()> {
    if let Value::Object(map) = value
        && map.len() == 1
    {
        value = map.values().next().unwrap_or(value);
    }
    match value {
        Value::Null => writeln!(out, "ok"),
        Value::Bool(value) => writeln!(out, "{value}"),
        Value::Number(value) => writeln!(out, "{value}"),
        Value::String(value) => writeln!(out, "{value}"),
        Value::Array(items)
            if !items.is_empty() && items.iter().all(|item| item.as_object().is_some()) =>
        {
            render_table(items, out)
        }
        other => {
            serde_json::to_writer_pretty(&mut out, other)?;
            writeln!(out)
        }
    }
}

fn render_table(items: &[Value], mut out: impl Write) -> io::Result<()> {
    let mut columns = Vec::<String>::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        for (key, value) in object {
            if is_scalar(value) && !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    if columns.is_empty() {
        serde_json::to_writer_pretty(&mut out, items)?;
        return writeln!(out);
    }

    let widths = column_widths(items, &columns);
    write_row(&mut out, &columns, &widths)?;
    let separators = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>();
    write_row(&mut out, &separators, &widths)?;
    for item in items {
        let object = item.as_object().expect("table items checked above");
        let cells = columns
            .iter()
            .map(|column| scalar_text(object.get(column).unwrap_or(&Value::Null)))
            .collect::<Vec<_>>();
        write_row(&mut out, &cells, &widths)?;
    }
    Ok(())
}

fn column_widths(items: &[Value], columns: &[String]) -> Vec<usize> {
    columns
        .iter()
        .map(|column| {
            let values = items.iter().filter_map(|item| {
                item.as_object()
                    .and_then(|object| object.get(column))
                    .map(scalar_text)
            });
            values
                .map(|value| value.chars().count())
                .chain(std::iter::once(column.chars().count()))
                .max()
                .unwrap_or(1)
                .min(72)
        })
        .collect()
}

fn write_row(out: &mut impl Write, cells: &[String], widths: &[usize]) -> io::Result<()> {
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index > 0 {
            write!(out, "  ")?;
        }
        let truncated = truncate(cell, *width);
        write!(out, "{truncated:width$}", width = width)?;
    }
    writeln!(out)
}

fn truncate(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    value.chars().take(width - 1).collect::<String>() + "…"
}

fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncates_by_unicode_scalar_without_breaking_utf8() {
        assert_eq!(truncate("供应商名称", 4), "供应商…");
        assert_eq!(truncate("ok", 4), "ok");
    }
}
