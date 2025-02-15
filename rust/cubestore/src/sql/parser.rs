use sqlparser::ast::{
    HiveDistributionStyle, Ident, ObjectName, Query, SqlOption, Statement as SQLStatement,
};
use sqlparser::dialect::keywords::Keyword;
use sqlparser::dialect::Dialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, Tokenizer};

#[derive(Debug)]
pub struct MySqlDialectWithBackTicks {}

impl Dialect for MySqlDialectWithBackTicks {
    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        ch == '"' || ch == '`'
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        (ch >= 'a' && ch <= 'z')
            || (ch >= 'A' && ch <= 'Z')
            || ch == '_'
            || ch == '$'
            || (ch >= '\u{0080}' && ch <= '\u{ffff}')
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        self.is_identifier_start(ch) || (ch >= '0' && ch <= '9')
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartitionedIndexRef {
    pub name: ObjectName,
    pub columns: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Statement(SQLStatement),
    CreateTable {
        create_table: SQLStatement,
        partitioned_index: Option<PartitionedIndexRef>,
        indexes: Vec<SQLStatement>,
        locations: Option<Vec<String>>,
        unique_key: Option<Vec<Ident>>,
    },
    CreateSchema {
        schema_name: ObjectName,
        if_not_exists: bool,
    },
    CreateSource {
        name: Ident,
        source_type: String,
        credentials: Vec<SqlOption>,
        or_update: bool,
    },
    Dump(Box<Query>),
}

pub struct CubeStoreParser<'a> {
    parser: Parser<'a>,
}

impl<'a> CubeStoreParser<'a> {
    pub fn new(sql: &str) -> Result<Self, ParserError> {
        let dialect = &MySqlDialectWithBackTicks {};
        let mut tokenizer = Tokenizer::new(dialect, sql);
        let tokens = tokenizer.tokenize()?;
        Ok(CubeStoreParser {
            parser: Parser::new(tokens, dialect),
        })
    }

    pub fn parse_statement(&mut self) -> Result<Statement, ParserError> {
        match self.parser.peek_token() {
            Token::Word(w) => match w.keyword {
                Keyword::CREATE => {
                    self.parser.next_token();
                    self.parse_create()
                }
                _ if w.value.eq_ignore_ascii_case("dump") => {
                    self.parser.next_token();
                    let s = self.parser.parse_statement()?;
                    let q = match s {
                        SQLStatement::Query(q) => q,
                        _ => {
                            return Err(ParserError::ParserError(
                                "Expected select query after 'dump'".to_string(),
                            ))
                        }
                    };
                    Ok(Statement::Dump(q))
                }
                _ => Ok(Statement::Statement(self.parser.parse_statement()?)),
            },
            _ => Ok(Statement::Statement(self.parser.parse_statement()?)),
        }
    }

    pub fn parse_create(&mut self) -> Result<Statement, ParserError> {
        if self.parser.parse_keyword(Keyword::SCHEMA) {
            self.parse_create_schema()
        } else if self.parser.parse_keyword(Keyword::TABLE) {
            self.parse_create_table()
        } else if self.parser.consume_token(&Token::make_keyword("SOURCE"))
            || self.parser.consume_token(&Token::make_keyword("source"))
        {
            self.parse_create_source()
        } else {
            Ok(Statement::Statement(self.parser.parse_create()?))
        }
    }

    pub fn parse_create_table(&mut self) -> Result<Statement, ParserError> {
        // Note that we disable hive extensions as they clash with `location`.
        let statement = self.parser.parse_create_table_ext(false, false, false)?;
        if let SQLStatement::CreateTable {
            name,
            columns,
            constraints,
            with_options,
            if_not_exists,
            file_format,
            query,
            without_rowid,
            or_replace,
            table_properties,
            like,
            ..
        } = statement
        {
            let unique_key = if self.parser.parse_keywords(&[Keyword::UNIQUE, Keyword::KEY]) {
                self.parser.expect_token(&Token::LParen)?;
                let res = Some(
                    self.parser
                        .parse_comma_separated(|p| p.parse_identifier())?,
                );
                self.parser.expect_token(&Token::RParen)?;
                res
            } else {
                None
            };

            let mut indexes = Vec::new();

            while self.parser.parse_keyword(Keyword::INDEX) {
                indexes.push(self.parse_with_index(name.clone())?);
            }

            let partitioned_index = if self.parser.parse_keywords(&[
                Keyword::ADD,
                Keyword::TO,
                Keyword::PARTITIONED,
                Keyword::INDEX,
            ]) {
                let name = self.parser.parse_object_name()?;
                self.parser.expect_token(&Token::LParen)?;
                let columns = self
                    .parser
                    .parse_comma_separated(Parser::parse_identifier)?;
                self.parser.expect_token(&Token::RParen)?;
                Some(PartitionedIndexRef { name, columns })
            } else {
                None
            };

            let locations = if self.parser.parse_keyword(Keyword::LOCATION) {
                Some(
                    self.parser
                        .parse_comma_separated(|p| p.parse_literal_string())?,
                )
            } else {
                None
            };

            Ok(Statement::CreateTable {
                create_table: SQLStatement::CreateTable {
                    or_replace,
                    name,
                    columns,
                    constraints,
                    hive_distribution: HiveDistributionStyle::NONE,
                    hive_formats: None,
                    table_properties,
                    with_options,
                    if_not_exists,
                    external: locations.is_some(),
                    file_format,
                    location: None,
                    query,
                    without_rowid,
                    temporary: false,
                    like,
                },
                indexes,
                partitioned_index,
                locations,
                unique_key,
            })
        } else {
            Ok(Statement::Statement(statement))
        }
    }

    pub fn parse_with_index(
        &mut self,
        table_name: ObjectName,
    ) -> Result<SQLStatement, ParserError> {
        let index_name = self.parser.parse_object_name()?;
        self.parser.expect_token(&Token::LParen)?;
        let columns = self
            .parser
            .parse_comma_separated(Parser::parse_order_by_expr)?;
        self.parser.expect_token(&Token::RParen)?;
        Ok(SQLStatement::CreateIndex {
            name: index_name,
            table_name,
            columns,
            unique: false,
            if_not_exists: false,
        })
    }

    fn parse_create_schema(&mut self) -> Result<Statement, ParserError> {
        let if_not_exists =
            self.parser
                .parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let schema_name = self.parser.parse_object_name()?;
        Ok(Statement::CreateSchema {
            schema_name,
            if_not_exists,
        })
    }

    fn parse_create_source(&mut self) -> Result<Statement, ParserError> {
        let or_update = self.parser.parse_keywords(&[Keyword::OR, Keyword::UPDATE]);
        let name = self.parser.parse_identifier()?;
        self.parser.expect_keyword(Keyword::AS)?;
        let source_type = self.parser.parse_literal_string()?;
        let credentials = self.parser.parse_options(Keyword::VALUES)?;
        Ok(Statement::CreateSource {
            name,
            or_update,
            credentials,
            source_type,
        })
    }
}
