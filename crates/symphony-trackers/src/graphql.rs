//! A minimal reader for GraphQL executable documents, used by `linear_graphql` to enforce the
//! `tracker.agent_tools.linear_mutations` allow-list (2026-10-25 audit, item A2).
//!
//! It tokenizes the document per the GraphQL lexical grammar (names, punctuators, strings and
//! block strings, numbers, comments, commas as whitespace) and reads operation and fragment
//! definitions far enough to know, for every `mutation` operation, which root fields it selects —
//! through aliases (`alias: field`), named fragment spreads and inline fragments. Field arguments,
//! variables and nested selections are skipped as balanced token groups. Anything it cannot read
//! (type-system definitions, unknown fragments, unbalanced braces) is an error, and the tool then
//! refuses the call rather than guessing.

use std::collections::{HashMap, HashSet};
use std::fmt;

/// The root-level effect of a document's operations.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DocumentSummary {
    /// Root fields selected by mutation operations (fragments expanded), in document order.
    pub mutation_fields: Vec<String>,
    /// The document defines a `subscription` operation.
    pub has_subscription: bool,
}

/// Why a document could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn error<T>(message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError(message.into()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Name(String),
    Punct(&'static str),
    /// A string, block string or number (their content never matters here).
    Value,
}

const PUNCTUATORS: [&str; 13] = [
    "!", "$", "&", "(", ")", ":", "=", "@", "[", "]", "{", "|", "}",
];

fn tokenize(source: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' | ',' | '\u{feff}' => i += 1,
            '#' => {
                while i < chars.len() && chars[i] != '\n' && chars[i] != '\r' {
                    i += 1;
                }
            }
            '"' if chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') => {
                i += 3;
                loop {
                    match chars.get(i) {
                        None => return error("unterminated block string"),
                        Some('\\')
                            if chars.get(i + 1) == Some(&'"')
                                && chars.get(i + 2) == Some(&'"')
                                && chars.get(i + 3) == Some(&'"') =>
                        {
                            i += 4;
                        }
                        Some('"')
                            if chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') =>
                        {
                            i += 3;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                tokens.push(Token::Value);
            }
            '"' => {
                i += 1;
                loop {
                    match chars.get(i) {
                        None | Some('\n') | Some('\r') => return error("unterminated string"),
                        Some('\\') => i += 2,
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                tokens.push(Token::Value);
            }
            '.' => {
                if chars.get(i + 1) == Some(&'.') && chars.get(i + 2) == Some(&'.') {
                    tokens.push(Token::Punct("..."));
                    i += 3;
                } else {
                    return error("unexpected `.`");
                }
            }
            c if c == '_' || c.is_ascii_alphabetic() => {
                let start = i;
                while i < chars.len() && (chars[i] == '_' || chars[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                tokens.push(Token::Name(chars[start..i].iter().collect()));
            }
            c if c == '-' || c.is_ascii_digit() => {
                i += 1;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '+' | '-'))
                {
                    i += 1;
                }
                tokens.push(Token::Value);
            }
            c => match PUNCTUATORS.iter().find(|p| p.starts_with(c)) {
                Some(p) => {
                    tokens.push(Token::Punct(p));
                    i += 1;
                }
                None => return error(format!("unexpected character {c:?}")),
            },
        }
    }
    Ok(tokens)
}

/// One selection at some level of a selection set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selection {
    Field(String),
    Spread(String),
    Inline(Vec<Selection>),
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_punct(&self, punct: &str) -> bool {
        matches!(self.peek(), Some(Token::Punct(p)) if *p == punct)
    }

    fn peek_name(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Token::Name(n)) if n == name)
    }

    fn expect_punct(&mut self, punct: &str) -> Result<(), ParseError> {
        if self.peek_punct(punct) {
            self.pos += 1;
            Ok(())
        } else {
            error(format!("expected `{punct}`"))
        }
    }

    fn expect_name(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(Token::Name(name)) => {
                let name = name.clone();
                self.pos += 1;
                Ok(name)
            }
            _ => error("expected a name"),
        }
    }

    /// Skips a balanced `open ... close` group starting at the current token.
    fn skip_group(&mut self, open: &str, close: &str) -> Result<(), ParseError> {
        self.expect_punct(open)?;
        let mut depth = 1usize;
        while depth > 0 {
            match self.peek() {
                None => return error(format!("unbalanced `{open}`")),
                Some(Token::Punct(p)) if *p == open => depth += 1,
                Some(Token::Punct(p)) if *p == close => depth -= 1,
                _ => {}
            }
            self.pos += 1;
        }
        Ok(())
    }

    fn skip_directives(&mut self) -> Result<(), ParseError> {
        while self.peek_punct("@") {
            self.pos += 1;
            self.expect_name()?;
            if self.peek_punct("(") {
                self.skip_group("(", ")")?;
            }
        }
        Ok(())
    }

    fn selection_set(&mut self) -> Result<Vec<Selection>, ParseError> {
        self.expect_punct("{")?;
        let mut selections = Vec::new();
        loop {
            if self.peek_punct("}") {
                self.pos += 1;
                return Ok(selections);
            }
            if self.peek_punct("...") {
                self.pos += 1;
                if self.peek_name("on") {
                    self.pos += 1;
                    self.expect_name()?;
                    self.skip_directives()?;
                    selections.push(Selection::Inline(self.selection_set()?));
                } else if let Some(Token::Name(_)) = self.peek() {
                    let name = self.expect_name()?;
                    self.skip_directives()?;
                    selections.push(Selection::Spread(name));
                } else {
                    self.skip_directives()?;
                    selections.push(Selection::Inline(self.selection_set()?));
                }
                continue;
            }
            let mut field = self.expect_name()?;
            if self.peek_punct(":") {
                self.pos += 1;
                field = self.expect_name()?;
            }
            if self.peek_punct("(") {
                self.skip_group("(", ")")?;
            }
            self.skip_directives()?;
            if self.peek_punct("{") {
                self.selection_set()?;
            }
            selections.push(Selection::Field(field));
        }
    }
}

fn root_fields(
    selections: &[Selection],
    fragments: &HashMap<String, Vec<Selection>>,
    visiting: &mut HashSet<String>,
    out: &mut Vec<String>,
) -> Result<(), ParseError> {
    for selection in selections {
        match selection {
            Selection::Field(name) => out.push(name.clone()),
            Selection::Inline(inner) => root_fields(inner, fragments, visiting, out)?,
            Selection::Spread(name) => {
                let Some(inner) = fragments.get(name) else {
                    return error(format!("unknown fragment {name}"));
                };
                if !visiting.insert(name.clone()) {
                    return error(format!("fragment {name} spreads itself"));
                }
                root_fields(inner, fragments, visiting, out)?;
                visiting.remove(name);
            }
        }
    }
    Ok(())
}

/// Reads `document` and reports what its operations do at the root.
pub fn summarize(document: &str) -> Result<DocumentSummary, ParseError> {
    let mut parser = Parser {
        tokens: tokenize(document)?,
        pos: 0,
    };
    let mut fragments = HashMap::new();
    let mut mutations = Vec::new();
    let mut summary = DocumentSummary::default();
    let mut definitions = 0usize;
    while parser.peek().is_some() {
        definitions += 1;
        if parser.peek_punct("{") {
            parser.selection_set()?;
            continue;
        }
        match parser.expect_name()?.as_str() {
            kind @ ("query" | "mutation" | "subscription") => {
                if let Some(Token::Name(_)) = parser.peek() {
                    parser.pos += 1;
                }
                if parser.peek_punct("(") {
                    parser.skip_group("(", ")")?;
                }
                parser.skip_directives()?;
                let selections = parser.selection_set()?;
                match kind {
                    "mutation" => mutations.push(selections),
                    "subscription" => summary.has_subscription = true,
                    _ => {}
                }
            }
            "fragment" => {
                let name = parser.expect_name()?;
                if name == "on" || !parser.peek_name("on") {
                    return error("malformed fragment definition");
                }
                parser.pos += 1;
                parser.expect_name()?;
                parser.skip_directives()?;
                let selections = parser.selection_set()?;
                fragments.insert(name, selections);
            }
            other => return error(format!("unsupported definition `{other}`")),
        }
    }
    if definitions == 0 {
        return error("empty document");
    }
    for selections in &mutations {
        root_fields(
            selections,
            &fragments,
            &mut HashSet::new(),
            &mut summary.mutation_fields,
        )?;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutations(document: &str) -> Vec<String> {
        summarize(document).unwrap().mutation_fields
    }

    #[test]
    fn queries_have_no_mutation_fields() {
        assert!(mutations("query Viewer { viewer { id } }").is_empty());
        assert!(mutations("{ x }").is_empty());
        assert!(mutations("query Q($id: String!) @live { issue(id: $id) { id } }").is_empty());
    }

    #[test]
    fn mutation_root_fields_resolve_aliases() {
        assert_eq!(
            mutations(
                r#"mutation M($id: String!) { a: commentCreate(input: {issueId: $id, body: "x"}) { success } issueUpdate(id: $id, input: {}) { issue { id } } }"#
            ),
            ["commentCreate", "issueUpdate"]
        );
        assert_eq!(
            mutations("mutation { renamed : issueDelete(id: 1) { success } }"),
            ["issueDelete"]
        );
    }

    #[test]
    fn every_operation_of_a_document_is_read() {
        let document = "query A { viewer { id } }\nmutation B { issueDelete(id: \"1\") { success } }\nmutation { commentCreate(input: {}) { success } }";
        assert_eq!(mutations(document), ["issueDelete", "commentCreate"]);
    }

    #[test]
    fn fragments_and_inline_fragments_are_expanded() {
        let document = "mutation { ...Bad ... on Mutation { issueArchive(id: 1) { success } } ... @include(if: true) { teamDelete(id: 1) { success } } }\nfragment Bad on Mutation { projectDelete(id: 1) { success } ...Nested }\nfragment Nested on Mutation { userSuspend(id: 1) { success } }";
        assert_eq!(
            mutations(document),
            ["projectDelete", "userSuspend", "issueArchive", "teamDelete"]
        );
        // Fragments used only by queries do not count.
        assert!(mutations("query { ...F }\nfragment F on Query { viewer { id } }").is_empty());
    }

    #[test]
    fn strings_and_comments_do_not_hide_or_fake_operations() {
        let document = "# mutation { issueDelete }\nquery { search(text: \"mutation { issueDelete(id: 1) }\") { id } }";
        assert!(mutations(document).is_empty());
        let block = "query { s(text: \"\"\"\nmutation { teamDelete }\n\\\"\"\" still inside\n\"\"\") { id } }\nmutation { issueDelete(id: 1) { success } }";
        assert_eq!(mutations(block), ["issueDelete"]);
        assert_eq!(mutations("mutation { a(n: -1.5e3, l: [1, 2]) }"), ["a"]);
    }

    #[test]
    fn subscriptions_are_reported() {
        assert!(
            summarize("subscription S { issueUpdates { id } }")
                .unwrap()
                .has_subscription
        );
        assert!(!summarize("query { x }").unwrap().has_subscription);
    }

    #[test]
    fn unreadable_documents_are_errors() {
        for document in [
            "",
            "   # only a comment",
            "mutation { a(",
            "mutation { a { b }",
            "type Query { a: String }",
            "mutation { ...Missing }",
            "mutation { ...A }\nfragment A on Mutation { ...A }",
            "query { a(s: \"unterminated) }",
            "query { a } %",
            "fragment on on X { a }",
        ] {
            assert!(summarize(document).is_err(), "{document:?}");
        }
    }
}
