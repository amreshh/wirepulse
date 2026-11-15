//! Syntax highlighting for the YAML views (policy and endpoint details).
//!
//! The backend writes that YAML with serde_yaml_ng, which uses a small part
//! of YAML: block mappings and lists with one key or item per line, quoted
//! strings that never wrap, `{}` and `[]` for empty collections, literal
//! blocks (`|-`) for strings with line breaks, and no comments (only the
//! one-line messages, like `# not available`, shown instead of an object).
//! So it can be coloured line by line, without a YAML parser.

use leptos::prelude::*;

/// `text`, coloured as YAML.
#[component]
pub fn Yaml(text: String) -> impl IntoView {
    let pieces = pieces(&text)
        .into_iter()
        .map(|(kind, s)| match kind.class() {
            Some(class) => view! { <span class=class>{s.to_string()}</span> }.into_any(),
            None => s.to_string().into_any(),
        })
        .collect_view();
    view! { <pre class="yaml">{pieces}</pre> }
}

/// What a piece of the text is, which picks its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Indentation, spaces and line breaks.
    Plain,
    Key,
    /// String values, quoted or not, and the lines of literal blocks.
    Str,
    /// Numbers, booleans and null.
    Literal,
    /// `-`, `:`, `{}`, `[]` and literal block headers like `|-`.
    Punct,
    Comment,
}

impl Kind {
    /// The class in style.css with its colour; none for plain text.
    fn class(self) -> Option<&'static str> {
        match self {
            Kind::Plain => None,
            Kind::Key => Some("y-key"),
            Kind::Str => Some("y-str"),
            Kind::Literal => Some("y-lit"),
            Kind::Punct => Some("y-punct"),
            Kind::Comment => Some("y-comment"),
        }
    }
}

/// `text` split into the pieces that make it up, in order.
fn pieces(text: &str) -> Vec<(Kind, &str)> {
    let mut out = Pieces(Vec::new());
    // In a literal block: the column of the key or `-` it belongs to. The
    // block's lines are the ones indented more than that, and blank ones.
    let mut block = None;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let indent = body.len() - body.trim_start_matches(' ').len();
        if block.is_some_and(|col| indent > col || indent == body.len()) {
            let rest = out.spaces(body);
            out.push(Kind::Str, rest);
        } else {
            block = out.line(body);
        }
        out.push(Kind::Plain, &line[body.len()..]);
    }
    out.0
}

/// The pieces so far, without empty ones.
struct Pieces<'a>(Vec<(Kind, &'a str)>);

impl<'a> Pieces<'a> {
    fn push(&mut self, kind: Kind, s: &'a str) {
        if !s.is_empty() {
            self.0.push((kind, s));
        }
    }

    /// Adds the spaces `s` starts with, and returns what follows them.
    fn spaces(&mut self, s: &'a str) -> &'a str {
        let rest = s.trim_start_matches(' ');
        self.push(Kind::Plain, &s[..s.len() - rest.len()]);
        rest
    }

    /// Adds a line that isn't in a literal block. If it starts one, returns
    /// the column of the key or `-` the block belongs to.
    fn line(&mut self, line: &'a str) -> Option<usize> {
        let mut rest = self.spaces(line);
        if rest.starts_with('#') {
            self.push(Kind::Comment, rest);
            return None;
        }
        let mut owner = line.len() - rest.len();
        // List items, maybe one in another (`- - a`).
        while let Some(after) = rest.strip_prefix('-').filter(|a| a.is_empty() || a.starts_with(' ')) {
            owner = line.len() - rest.len();
            self.push(Kind::Punct, "-");
            rest = self.spaces(after);
        }
        if let Some(end) = key_end(rest) {
            owner = line.len() - rest.len();
            self.push(Kind::Key, &rest[..end]);
            self.push(Kind::Punct, ":");
            rest = self.spaces(&rest[end + 1..]);
        }
        self.push(value_kind(rest), rest);
        rest.starts_with(['|', '>']).then_some(owner)
    }
}

/// If `s` starts with a key, the index of the `:` after it.
fn key_end(s: &str) -> Option<usize> {
    let end = if s.starts_with(['\'', '"']) {
        quoted_len(s)
    } else {
        // Unquoted keys can't contain ": ", so the first one ends the key.
        s.find(": ").or_else(|| s.strip_suffix(':').map(str::len))?
    };
    let after = &s[end..];
    (after == ":" || after.starts_with(": ")).then_some(end)
}

/// The length of the quoted string `s` starts with, quotes included (all of
/// `s` if it doesn't end).
fn quoted_len(s: &str) -> usize {
    let b = s.as_bytes();
    let quote = b[0];
    let mut i = 1;
    while i < b.len() {
        if quote == b'"' && b[i] == b'\\' {
            i += 2; // an escape, maybe of a quote
        } else if b[i] != quote {
            i += 1;
        } else if quote == b'\'' && b.get(i + 1) == Some(&b'\'') {
            i += 2; // '' is a quote inside single quotes
        } else {
            return i + 1;
        }
    }
    s.len()
}

/// The colour of the value that ends a line.
fn value_kind(v: &str) -> Kind {
    match v {
        _ if v.starts_with(['|', '>']) => Kind::Punct,
        _ if v.starts_with(['\'', '"']) => Kind::Str,
        "{}" | "[]" => Kind::Punct,
        // serde_yaml_ng quotes strings that would read as anything else, so
        // these are what they look like. Like it, take only finite floats:
        // `inf`, `nan` and `1e400` are strings.
        "true" | "false" | "null" => Kind::Literal,
        _ if v.parse::<f64>().is_ok_and(f64::is_finite) => Kind::Literal,
        _ => Kind::Str,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text with its coloured pieces tagged: `<k>` key, `<s>` string,
    /// `<l>` literal, `<p>` punctuation, `<c>` comment.
    fn tagged(text: &str) -> String {
        pieces(text)
            .into_iter()
            .map(|(kind, s)| {
                let tag = match kind {
                    Kind::Plain => return s.to_string(),
                    Kind::Key => "k",
                    Kind::Str => "s",
                    Kind::Literal => "l",
                    Kind::Punct => "p",
                    Kind::Comment => "c",
                };
                format!("<{tag}>{s}</{tag}>")
            })
            .collect()
    }

    #[test]
    fn keys_and_values() {
        assert_eq!(
            tagged("apiVersion: cilium.io/v2\nmetadata:\n  generation: 1\n"),
            "<k>apiVersion</k><p>:</p> <s>cilium.io/v2</s>\n<k>metadata</k><p>:</p>\n  <k>generation</k><p>:</p> <l>1</l>\n"
        );
        // Selector keys like `k8s:app` have colons in them.
        assert_eq!(tagged("k8s:app: web"), "<k>k8s:app</k><p>:</p> <s>web</s>");
        assert_eq!(tagged("k8s:app:"), "<k>k8s:app</k><p>:</p>");
        assert_eq!(tagged("url: https://a.example/x#y"), "<k>url</k><p>:</p> <s>https://a.example/x#y</s>");
    }

    #[test]
    fn quoted() {
        assert_eq!(tagged("port: '6443'"), "<k>port</k><p>:</p> <s>'6443'</s>");
        assert_eq!(tagged("'80': http"), "<k>'80'</k><p>:</p> <s>http</s>");
        assert_eq!(tagged("'': x"), "<k>''</k><p>:</p> <s>x</s>");
        assert_eq!(tagged("'it''s: x': 'a: b'"), "<k>'it''s: x'</k><p>:</p> <s>'a: b'</s>");
        assert_eq!(tagged(r#""a\"b: c": "d""#), r#"<k>"a\"b: c"</k><p>:</p> <s>"d"</s>"#);
        assert_eq!(tagged("- 'a: b'"), "<p>-</p> <s>'a: b'</s>");
    }

    #[test]
    fn literals() {
        for v in ["1", "-3", "0.5", "1e300", "true", "false", "null"] {
            assert_eq!(tagged(&format!("x: {v}")), format!("<k>x</k><p>:</p> <l>{v}</l>"));
        }
        // Strings that serde_yaml_ng leaves unquoted.
        for v in ["v1.2.3", "2026-11-15T18:35:28Z", "inf", "nan", "1e400", "1_000", "yes"] {
            assert_eq!(tagged(&format!("x: {v}")), format!("<k>x</k><p>:</p> <s>{v}</s>"));
        }
        assert_eq!(tagged("a: {}\nb: []"), "<k>a</k><p>:</p> <p>{}</p>\n<k>b</k><p>:</p> <p>[]</p>");
    }

    #[test]
    fn lists() {
        assert_eq!(
            tagged("egress:\n- toEntities:\n  - kube-apiserver\n- - a\n  - -1\n-\n"),
            "<k>egress</k><p>:</p>\n<p>-</p> <k>toEntities</k><p>:</p>\n  <p>-</p> <s>kube-apiserver</s>\n\
             <p>-</p> <p>-</p> <s>a</s>\n  <p>-</p> <l>-1</l>\n<p>-</p>\n"
        );
    }

    #[test]
    fn literal_blocks() {
        // The lines of a block are a string, even when they look like YAML.
        assert_eq!(
            tagged("data:\n  script: |-\n    key: value\n\n    - item\n  next: 1\n"),
            "<k>data</k><p>:</p>\n  <k>script</k><p>:</p> <p>|-</p>\n    <s>key: value</s>\n\n    <s>- item</s>\n\
             \x20 <k>next</k><p>:</p> <l>1</l>\n"
        );
        // In a list item, a block belongs to its key, not the `-`.
        assert_eq!(
            tagged("- name: |\n    a\n  port: 80\n- |2-\n    b\n  c\nend: true"),
            "<p>-</p> <k>name</k><p>:</p> <p>|</p>\n    <s>a</s>\n  <k>port</k><p>:</p> <l>80</l>\n\
             <p>-</p> <p>|2-</p>\n    <s>b</s>\n  <s>c</s>\n<k>end</k><p>:</p> <l>true</l>"
        );
    }

    #[test]
    fn comments() {
        assert_eq!(tagged("# could not render: x"), "<c># could not render: x</c>");
        assert_eq!(tagged("# not available"), "<c># not available</c>");
    }

    #[test]
    fn keeps_all_the_text() {
        for text in ["", "\n\n", "'", "\"\\", "- ", "-", ":", "é:", "x: é\n", "'é", "a: |\n", "  ", "a\r\nb"] {
            let joined: String = pieces(text).into_iter().map(|(_, s)| s).collect();
            assert_eq!(joined, text);
        }
    }
}
