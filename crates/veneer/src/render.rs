//! Human diagnostic rendering through miette.

use std::fmt;
use std::io::IsTerminal;
use std::path::Path;

use miette::{GraphicalReportHandler, GraphicalTheme, LabeledSpan, NamedSource, SourceCode};
use suspect_source::Uri;
use veneer_spec::{Diagnostic, Severity};

struct Rendered {
    diag: Diagnostic,
    source: Option<NamedSource<String>>,
}

impl fmt::Debug for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diag.message)
    }
}

impl fmt::Display for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diag.message)
    }
}

impl std::error::Error for Rendered {}

impl miette::Diagnostic for Rendered {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        Some(Box::new(self.diag.code))
    }

    fn severity(&self) -> Option<miette::Severity> {
        Some(match self.diag.severity {
            Severity::Error => miette::Severity::Error,
            Severity::Warning => miette::Severity::Warning,
        })
    }

    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        (!self.diag.notes.is_empty())
            .then(|| Box::new(self.diag.notes.join("\n")) as Box<dyn fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.source.as_ref().map(|s| s as &dyn SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let span = self.diag.span.as_ref()?;
        self.source.as_ref()?;
        let len = span.end.saturating_sub(span.start);
        Some(Box::new(std::iter::once(LabeledSpan::at(
            span.start..span.start + len,
            "here",
        ))))
    }
}

/// `path` relative to `cwd` when it is inside it.
pub fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

pub fn render(diags: &[Diagnostic], cwd: &Path) -> String {
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let theme = if color {
        GraphicalTheme::unicode()
    } else {
        GraphicalTheme::unicode_nocolor()
    };
    let handler = GraphicalReportHandler::new_themed(theme).with_width(100);
    let mut out = String::new();
    for diag in diags {
        let source = diag.span.as_ref().and_then(|span| {
            let path = Uri::parse(&span.uri).ok()?.as_path()?;
            let text = std::fs::read_to_string(&path).ok()?;
            Some(NamedSource::new(display_path(&path, cwd), text))
        });
        let rendered = Rendered {
            diag: diag.clone(),
            source,
        };
        handler
            .render_report(&mut out, &rendered)
            .expect("writing to a String cannot fail");
    }
    out
}
