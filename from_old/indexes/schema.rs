use std::sync::Arc;

use fancy_regex::Regex;
use tantivy::schema::{
    BytesOptions, Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, STORED,
    STRING,
};
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

use crate::chunking::languages::TSLanguageParsing;
use crate::{db::sqlite::SqlDb, semantic_search::client::SemanticClient};

/// A schema for indexing all files and directories, linked to a
/// single repository on disk.
#[derive(Clone)]
pub struct File {
    pub schema: Schema,
    pub(super) semantic: Option<SemanticClient>,
    /// Unique ID for the file in a repo
    pub unique_hash: Field,

    pub sql: SqlDb,

    /// Path to the root of the repo on disk
    pub repo_disk_path: Field,
    /// Path to the file, relative to the repo root
    pub relative_path: Field,

    /// Unique repo identifier, of the form:
    ///  local: local//path/to/repo
    /// github: github.com/org/repo
    pub repo_ref: Field,

    /// Indexed repo name, of the form:
    ///  local: repo
    /// github: github.com/org/repo
    pub repo_name: Field,

    pub content: Field,
    pub line_end_indices: Field,
    // / a flat list of every symbol's text, for searching, e.g.:
    // / ["File", "Repo", "worker"]
    pub symbols: Field,

    /// fast fields for scoring
    pub lang: Field,
    pub avg_line_length: Field,
    pub last_commit_unix_seconds: Field,

    /// fast byte versions of certain fields for collector-level filtering
    pub raw_content: Field,
    pub raw_repo_name: Field,
    pub raw_relative_path: Field,

    /// list of branches in which this file can be found
    pub branches: Field,

    /// Whether this entry is a file or a directory
    pub is_directory: Field,
    // How many commits have been made to this file in last 2 weeks
    pub commit_frequency: Field,

    // The commit hash for this file
    pub commit_hash: Field,
}

impl File {
    pub fn new(sql: SqlDb, semantic: Option<SemanticClient>) -> Self {
        let mut builder = tantivy::schema::SchemaBuilder::new();
        let trigram = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );

        let unique_hash = builder.add_text_field("unique_hash", STRING | STORED);

        let repo_disk_path = builder.add_text_field("repo_disk_path", STRING);
        let repo_ref = builder.add_text_field("repo_ref", STRING | STORED);
        let repo_name = builder.add_text_field("repo_name", trigram.clone());
        let relative_path = builder.add_text_field("relative_path", trigram.clone());

        let content = builder.add_text_field("content", trigram.clone());
        let line_end_indices =
            builder.add_bytes_field("line_end_indices", BytesOptions::default().set_stored());

        let symbols = builder.add_text_field("symbols", trigram.clone());

        let branches = builder.add_text_field("branches", trigram);

        let lang = builder.add_bytes_field(
            "lang",
            BytesOptions::default().set_stored().set_indexed() | FAST,
        );
        let avg_line_length = builder.add_f64_field("line_length", FAST);
        let last_commit_unix_seconds = builder.add_i64_field("last_commit_unix_seconds", FAST);

        let raw_content = builder.add_bytes_field("raw_content", FAST);
        let raw_repo_name = builder.add_bytes_field("raw_repo_name", FAST);
        let raw_relative_path = builder.add_bytes_field("raw_relative_path", FAST);

        let is_directory = builder.add_bool_field("is_directory", FAST);
        let commit_frequency = builder.add_u64_field("commit_frequency", FAST);
        let commit_hash = builder.add_text_field("commit_hash", STRING);

        Self {
            sql,
            semantic,
            repo_disk_path,
            relative_path,
            unique_hash,
            repo_ref,
            repo_name,
            last_commit_unix_seconds,
            schema: builder.build(),
            raw_repo_name,
            raw_relative_path,
            is_directory,
            content,
            line_end_indices,
            symbols,
            lang,
            avg_line_length,
            raw_content,
            branches,
            commit_frequency,
            commit_hash,
        }
    }
}

/// A schema for indexing all the generated code snippets, each code snippet
/// is linked to a single file
#[derive(Clone)]
pub struct CodeSnippet {
    pub schema: Schema,
    /// Unique ID for the file in a repo
    pub unique_hash: Field,

    pub language_parsing: Arc<TSLanguageParsing>,

    pub sql: SqlDb,

    /// Path to the root of the repo on disk
    pub repo_disk_path: Field,
    /// Path to the file, relative to the repo root
    pub relative_path: Field,

    /// Unique repo identifier, of the form:
    ///  local: local//path/to/repo
    /// github: github.com/org/repo
    pub repo_ref: Field,

    /// Indexed repo name, of the form:
    ///  local: repo
    /// github: github.com/org/repo
    pub repo_name: Field,

    pub content: Field,

    /// fast fields for scoring
    pub lang: Field,
    pub last_commit_unix_seconds: Field,

    /// fast byte versions of certain fields for collector-level filtering
    pub raw_content: Field,
    pub raw_repo_name: Field,
    pub raw_relative_path: Field,

    // How many commits have been made to this file in last 2 weeks
    pub commit_frequency: Field,

    // The commit hash for this file
    pub commit_hash: Field,

    // The start line of this code snippet
    pub start_line: Field,
    // The end line of this code snippet
    pub end_line: Field,
}

impl CodeSnippet {
    pub fn new(sql: SqlDb, language_parsing: Arc<TSLanguageParsing>) -> Self {
        let mut builder = tantivy::schema::SchemaBuilder::new();
        let code_snippet_tokenizer = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                // We get the code_snippet tokenizer from the custom
                // tokenizer we are setting
                .set_tokenizer("code_snippet")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );

        let unique_hash = builder.add_text_field("unique_hash", STRING | STORED);

        let repo_disk_path = dbg!(builder.add_text_field("repo_disk_path", STRING));
        let repo_ref = builder.add_text_field("repo_ref", STRING | STORED);
        let repo_name = builder.add_text_field("repo_name", code_snippet_tokenizer.clone());
        let relative_path = builder.add_text_field("relative_path", code_snippet_tokenizer.clone());

        let content = builder.add_text_field("content", code_snippet_tokenizer.clone());

        let lang = builder.add_bytes_field(
            "lang",
            BytesOptions::default().set_stored().set_indexed() | FAST,
        );
        let last_commit_unix_seconds = builder.add_i64_field("last_commit_unix_seconds", FAST);

        let raw_content = builder.add_bytes_field("raw_content", FAST);
        let raw_repo_name = builder.add_bytes_field("raw_repo_name", FAST);
        let raw_relative_path = builder.add_bytes_field("raw_relative_path", FAST);

        let commit_frequency = builder.add_u64_field("commit_frequency", FAST);
        let commit_hash = builder.add_text_field("commit_hash", STRING);
        let start_line = builder.add_u64_field("start_line", FAST | STORED);
        let end_line = builder.add_u64_field("end_line", FAST | STORED);

        Self {
            sql,
            language_parsing,
            repo_disk_path,
            relative_path,
            unique_hash,
            repo_ref,
            repo_name,
            last_commit_unix_seconds,
            schema: builder.build(),
            raw_repo_name,
            raw_relative_path,
            content,
            lang,
            raw_content,
            commit_frequency,
            commit_hash,
            start_line,
            end_line,
        }
    }
}

#[derive(Clone)]
pub struct CodeSnippetTokenizer {}

#[derive(Clone)]
pub struct CodeSnippetTokenizerStream<'a> {
    /// input
    _text: &'a str,
    /// current position
    position: Option<usize>,
    // What are the processed tokens for this text
    tokens: Vec<Token>,
}

impl Tokenizer for CodeSnippetTokenizer {
    type TokenStream<'a> = CodeSnippetTokenizerStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let tokens = get_code_tokens_for_string(text);
        CodeSnippetTokenizerStream {
            _text: text,
            // we start with none here because the comparisons are all with
            // usize, so its better to just say None to represent -1
            position: None,
            tokens,
        }
    }
}

impl CodeSnippetTokenizer {
    pub fn tokenize_call(query: &str) -> Vec<Token> {
        let tokens = get_code_tokens_for_string(query);
        tokens
    }
}

impl<'a> TokenStream for CodeSnippetTokenizerStream<'a> {
    /// advances to the next token or returns false if there is no token here
    fn advance(&mut self) -> bool {
        self.position = match self.position {
            Some(position) => Some(position + 1),
            None => Some(0),
        };
        if self.position.expect("check above converts it to Some") >= self.tokens.len() {
            return false;
        }
        // otherwise we increment the counter here
        true
    }

    /// Returns a reference to the current token
    fn token(&self) -> &tantivy::tokenizer::Token {
        // We know its unsafe but this will never crash because we are extremely
        // careful when taking the position into account
        &self.tokens[self.position.expect("token is always called after advance")]
    }

    /// Returns a mutable reference to the current token.
    fn token_mut(&mut self) -> &mut tantivy::tokenizer::Token {
        // same comment as fn token, this won't crash as we are going to be
        // extremely careful with the way we handle position
        &mut self.tokens[self.position.expect("token is always called after advance")]
    }
}

/// First we try to tokenize the whole string in 1 go and get back the range
/// of tokens, and then we return them one after the other
fn check_valid_token(token: &str) -> bool {
    token.len() > 1
}

fn tokenize_call(code: &str) -> Vec<Token> {
    let re = Regex::new(r"\b\w+\b").unwrap();
    let mut pos = 0;
    let mut valid_tokens = Vec::new();

    for m in re.find_iter(code) {
        let text = m.expect("regex_parsing to not fail").as_str();

        if text.contains('_') {
            // snake_case
            let parts: Vec<&str> = text.split('_').collect();
            for part in parts {
                if check_valid_token(part) {
                    valid_tokens.push(Token {
                        offset_from: 0,
                        offset_to: part.len(),
                        position: pos,
                        text: part.to_lowercase(),
                        position_length: 1,
                    });
                    pos += 1;
                }
            }
        } else if text.chars().any(|c| c.is_uppercase()) {
            // PascalCase and camelCase
            let camel_re = Regex::new(r"[A-Z][a-z]+|[a-z]+|[A-Z]+(?=[A-Z]|$)").unwrap();
            let parts: Vec<&str> = camel_re
                .find_iter(text)
                .map(|mat| mat.expect("regex parsing to not fail").as_str())
                .collect();
            for part in parts {
                if check_valid_token(part) {
                    valid_tokens.push(Token {
                        offset_from: 0,
                        offset_to: part.len(),
                        position: pos,
                        text: part.to_lowercase(),
                        position_length: 1,
                    });
                    pos += 1;
                }
            }
        } else {
            if check_valid_token(text) {
                valid_tokens.push(Token {
                    offset_from: 0,
                    offset_to: text.len(),
                    position: pos,
                    text: text.to_lowercase(),
                    position_length: 1,
                });
                pos += 1;
            }
        }
    }

    // Now we want to create the bigrams and the tigrams from these tokens
    // and have them stored too, so we can process them
    valid_tokens
}

fn create_bigrams(tokens: &[Token]) -> Vec<Token> {
    // when creating the bigrams we join the current and the previous token
    // using _
    let mut previous_token: Option<&Token> = None;
    let mut bigrams = Vec::new();
    for token in tokens {
        if let Some(prev_token) = previous_token {
            let bigram = format!("{}_{}", prev_token.text, token.text);
            bigrams.push(Token {
                offset_from: 0,
                offset_to: bigram.len(),
                position: prev_token.position,
                text: bigram,
                position_length: 1,
            });
        }
        previous_token = Some(token);
    }
    bigrams
}

fn create_trigrams(tokens: &[Token]) -> Vec<Token> {
    // when creating the trigrams here we have to do the same thing
    let mut previous_token: Option<&Token> = None;
    let mut previous_previous_token: Option<&Token> = None;
    let mut trigrams = Vec::new();
    for token in tokens {
        if let Some(prev_token) = previous_token {
            if let Some(prev_prev_token) = previous_previous_token {
                let trigram = format!(
                    "{}_{}_{}",
                    prev_prev_token.text, prev_token.text, token.text
                );
                trigrams.push(Token {
                    offset_from: 0,
                    offset_to: trigram.len(),
                    position: prev_prev_token.position,
                    text: trigram,
                    position_length: 1,
                });
            }
        }
        previous_previous_token = previous_token;
        previous_token = Some(token);
    }
    trigrams
}

fn get_code_tokens_for_string(text: &str) -> Vec<Token> {
    let mut tokens = tokenize_call(text);
    let bigrams = create_bigrams(tokens.as_slice());
    let trigrams = create_trigrams(tokens.as_slice());
    tokens.extend(bigrams);
    tokens.extend(trigrams);
    tokens
}
