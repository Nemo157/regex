use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::mem;
use std::result;

use ast::{self, Ast, Position, Span};
use either::Either;

type Result<T> = result::Result<T, ast::Error>;

/// A primitive is an expression with no sub-expressions. This includes
/// literals, assertions and non-set character classes. This representation
/// is used as intermediate state in the parser.
///
/// This does not include ASCII character classes, since they can only appear
/// within a set character class.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Primitive {
    Literal(ast::Literal),
    Assertion(ast::Assertion),
    Dot(Span),
    Perl(ast::ClassPerl),
    Unicode(ast::ClassUnicode),
}

impl Primitive {
    /// Return the span of this primitive.
    fn span(&self) -> &Span {
        match *self {
            Primitive::Literal(ref x) => &x.span,
            Primitive::Assertion(ref x) => &x.span,
            Primitive::Dot(ref span) => span,
            Primitive::Perl(ref x) => &x.span,
            Primitive::Unicode(ref x) => &x.span,
        }
    }

    /// Convert this primitive into a proper AST.
    fn into_ast(self) -> Ast {
        match self {
            Primitive::Literal(lit) => Ast::Literal(lit),
            Primitive::Assertion(assert) => Ast::Assertion(assert),
            Primitive::Dot(span) => Ast::Dot(span),
            Primitive::Perl(cls) => Ast::Class(ast::Class::Perl(cls)),
            Primitive::Unicode(cls) => Ast::Class(ast::Class::Unicode(cls)),
        }
    }

    /// Convert this primitive into an item in a character class.
    ///
    /// If this primitive is not a legal item (i.e., an assertion or a dot),
    /// then return an error.
    fn into_class_set_item(self) -> Result<ast::ClassSetItem> {
        use self::Primitive::*;

        match self {
            Literal(lit) => Ok(ast::ClassSetItem::Literal(lit)),
            Perl(cls) => {
                Ok(ast::ClassSetItem::Class(Box::new(ast::Class::Perl(cls))))
            }
            Unicode(cls) => {
                Ok(ast::ClassSetItem::Class(Box::new(ast::Class::Unicode(cls))))
            }
            x => Err(ast::Error {
                span: *x.span(),
                kind: ast::ErrorKind::ClassIllegal,
            })
        }
    }

    /// Convert this primitive into a literal in a character class. In
    /// particular, literals are the only valid items that can appear in
    /// ranges.
    ///
    /// If this primitive is not a legal item (i.e., a class, assertion or a
    /// dot), then return an error.
    fn into_class_literal(self) -> Result<ast::Literal> {
        use self::Primitive::*;

        match self {
            Literal(lit) => Ok(lit),
            x => Err(ast::Error {
                span: *x.span(),
                kind: ast::ErrorKind::ClassIllegal,
            })
        }
    }
}

/// Returns true if the give character has significance in a regex.
///
/// These are the only characters that are allowed to be escaped.
fn is_punct(c: char) -> bool {
    match c {
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' |
        '[' | ']' | '{' | '}' | '^' | '$' | '#' | '&' | '-' | '~' => true,
        _ => false,
    }
}

/// Returns true if the given character is a hexadecimal digit.
fn is_hex(c: char) -> bool {
    ('0' <= c && c <= '9') || ('a' <= c && c <= 'f') || ('A' <= c && c <= 'F')
}

/// Returns true if the given character is a valid in a capture group name.
///
/// If `first` is true, then `c` is treated as the first character in the
/// group name (which is not allowed to be a digit).
fn is_capture_char(c: char, first: bool) -> bool {
    c == '_' || (!first && c >= '0' && c <= '9')
    || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
}

/// A builder for a regular expression parser.
///
/// This builder permits modifying configuration options for the parser.
#[derive(Clone, Debug)]
pub struct ParserBuilder {
    ignore_space: bool,
    nest_limit: u32,
    octal: bool,
}

impl Default for ParserBuilder {
    fn default() -> ParserBuilder {
        ParserBuilder::new()
    }
}

impl ParserBuilder {
    /// Create a new parser builder with a default configuration.
    pub fn new() -> ParserBuilder {
        ParserBuilder {
            ignore_space: false,
            nest_limit: 100,
            octal: false,
        }
    }

    /// Build a parser from this configuration with the given pattern.
    pub fn build(&self) -> Parser {
        Parser {
            pos: Cell::new(Position { offset: 0, line: 1, column: 1 }),
            nest_limit: self.nest_limit,
            octal: self.octal,
            initial_ignore_space: self.ignore_space,
            ignore_space: Cell::new(self.ignore_space),
            comments: RefCell::new(vec![]),
            stack_group: RefCell::new(vec![]),
            stack_class: RefCell::new(vec![]),
        }
    }

    /// Enable verbose mode in the regular expression.
    ///
    /// When enabled, verbose mode permits insigificant whitespace in many
    /// places in the regular expression, as well as comments. Comments are
    /// started using `#` and continue until the end of the line.
    ///
    /// By default, this is disabled. It may be selectively enabled in the
    /// regular expression by using the `x` flag.
    pub fn ignore_space(&mut self, yes: bool) -> &mut ParserBuilder {
        self.ignore_space = yes;
        self
    }

    /// Set the nesting limit for this parser.
    ///
    /// The nesting limit controls how deep the abstract syntax tree is allowed
    /// to be. If the AST exceeds the given limit (e.g., with too many nested
    /// groups), then an error is returned by the parser.
    ///
    /// Note that a nest limit of `0` will return a nest limit error for every
    /// regular expression. A nest limit of `1` allows `a` but not `(a)` or
    /// `a+`.
    pub fn nest_limit(&mut self, limit: u32) -> &mut ParserBuilder {
        self.nest_limit = limit;
        self
    }

    /// Whether to support octal syntax or not.
    ///
    /// Octal syntax is a little-known way of uttering Unicode codepoints in
    /// a regular expression. For example, `a`, `\x61`, `\u0061` and
    /// `\141` are all equivalent regular expressions, where the last example
    /// shows octal syntax.
    ///
    /// While supporting octal syntax isn't in and of itself a problem, it does
    /// make good error messages harder. That is, in PCRE based regex engines,
    /// syntax like `\0` invokes a backreference, which is explicitly
    /// unsupported in Rust's regex engine. However, many users expect it to
    /// be supported. Therefore, when octal support is disabled, the error
    /// message will explicitly mention that backreferences aren't supported.
    ///
    /// Octal syntax is disabled by default.
    pub fn octal(&mut self, yes: bool) -> &mut ParserBuilder {
        self.octal = yes;
        self
    }
}

/// A regular expression parser.
///
/// This parses a string representation of a regular expression into an
/// abstract syntax tree. The size of the tree is proportional to the length
/// of the regular expression pattern.
#[derive(Clone, Debug)]
pub struct Parser {
    /// The current position of the parser.
    pos: Cell<Position>,
    /// The maximum number of open parens/brackets allowed. If the parser
    /// exceeds this number, then an error is returned.
    nest_limit: u32,
    /// Whether to support octal syntax or not. When `false`, the parser will
    /// return an error helpfully pointing out that backreferences are not
    /// supported.
    octal: bool,
    /// The initial setting for `ignore_space` as provided by `ParserBuilder`.
    /// This is used when reseting the parser's state.
    initial_ignore_space: bool,
    /// Whether whitespace should be ignored. When enabled, comments are
    /// also permitted.
    ignore_space: Cell<bool>,
    /// A list of comments, in order of appearance.
    comments: RefCell<Vec<ast::Comment>>,
    /// A stack of grouped sub-expressions, including alternations.
    stack_group: RefCell<Vec<GroupState>>,
    /// A stack of nested character classes. This is only non-empty when
    /// parsing a class.
    stack_class: RefCell<Vec<ClassState>>,
}

/// ParserI is the internal parser implementation.
///
/// We could elide this separate type and use a `RefCell<String>` for the
/// pattern instead, but a `String` is much more convenient (because you can
/// get a `&str` more easily).
#[derive(Clone, Debug)]
struct ParserI<P> {
    /// The full regular expression provided by the user.
    pattern: String,
    /// The parser state/configuration.
    parser: P,
}

/// GroupState represents a single stack frame while parsing nested groups
/// and alternations. Each frame records the state up to an opening parenthesis
/// or a alternating bracket `|`.
#[derive(Clone, Debug)]
enum GroupState {
    /// This state is pushed whenever an opening group is found.
    Group {
        /// The concatenation immediately preceding the opening group.
        concat: ast::Concat,
        /// The group that has been opened. Its sub-AST is always empty.
        group: ast::Group,
        /// Whether this group has the `x` flag enabled or not.
        ignore_space: bool,
    },
    /// This state is pushed whenever a new alternation branch is found. If
    /// an alternation branch is found and this state is at the top of the
    /// stack, then this state should be modified to include the new
    /// alternation.
    Alternation(ast::Alternation),
}

/// ClassState represents a single stack frame while parsing character classes.
/// Each frame records the state up to an intersection, difference, symmetric
/// difference or nested class.
///
/// Note that a parser's character class stack is only non-empty when parsing
/// a character class. In all other cases, it is empty.
#[derive(Clone, Debug)]
enum ClassState {
    /// This state is pushed whenever an opening bracket is found.
    Open {
        /// The union of class items immediately preceding this class.
        union: ast::ClassSetUnion,
        /// The class that has been opened. Typically this just corresponds
        /// to the `[`, but it can also include `[^` since `^` indicates
        /// negation of the class.
        set: ast::ClassSet,
    },
    /// This state is pushed when a operator is seen. When popped, the stored
    /// set becomes the left hand side of the operator.
    Op {
        /// The type of the operation, i.e., &&, -- or ~~.
        kind: ast::ClassSetBinaryOpKind,
        /// The left-hand side of the operator.
        lhs: ast::ClassSetOp,
    },
}

impl ClassState {
    /// Returns true if and only if this state corresponds to the opening of
    /// a character class set.
    fn is_open(&self) -> bool {
        match *self {
            ClassState::Open { .. } => true,
            ClassState::Op { .. } => false,
        }
    }
}

impl Parser {
    /// Create a new parser for the given regular expression.
    ///
    /// The parser can be run with either the `parse` or `parse_with_comments`
    /// methods. The parse methods return an abstract syntax tree.
    ///
    /// To set configuration options on the parser, use
    /// [`ParserBuilder`](struct.ParserBuilder.html).
    pub fn new() -> Parser {
        ParserBuilder::new().build()
    }

    /// Parse the regular expression into an abstract syntax tree.
    pub fn parse(&self, pattern: &str) -> Result<Ast> {
        ParserI::new(self, pattern).parse()
    }

    /// Parse the regular expression and return an abstract syntax tree with
    /// all of the comments found in the pattern.
    pub fn parse_with_comments(
        &self,
        pattern: &str,
    ) -> Result<ast::WithComments> {
        ParserI::new(self, pattern).parse_with_comments()
    }

    /// Reset the internal state of a parser.
    ///
    /// This is called at the beginning of every parse. This prevents the
    /// parser from running with inconsistent state (say, if a previous
    /// invocation returned an error and the parser is reused).
    fn reset(&self) {
        // These settings should be in line with the construction
        // in `ParserBuilder::build`.
        self.pos.set(Position { offset: 0, line: 1, column: 1});
        self.ignore_space.set(self.initial_ignore_space);
        self.comments.borrow_mut().clear();
        self.stack_group.borrow_mut().clear();
        self.stack_class.borrow_mut().clear();
    }
}

impl<P: Borrow<Parser>> ParserI<P> {
    /// Build an internal parser from a parser configuration and a pattern.
    fn new(parser: P, pattern: &str) -> ParserI<P> {
        ParserI { pattern: pattern.to_string(), parser: parser }
    }

    /// Return a reference to the parser state.
    fn parser(&self) -> &Parser {
        self.parser.borrow()
    }

    /// Return the current offset of the parser.
    ///
    /// The offset starts at `0` from the beginning of the regular expression
    /// pattern string.
    fn offset(&self) -> usize {
        self.parser().pos.get().offset
    }

    /// Return the current line number of the parser.
    ///
    /// The line number starts at `1`.
    fn line(&self) -> usize {
        self.parser().pos.get().line
    }

    /// Return the current column of the parser.
    ///
    /// The column number starts at `1` and is reset whenever a `\n` is seen.
    fn column(&self) -> usize {
        self.parser().pos.get().column
    }

    /// Return whether the parser should ignore whitespace or not.
    fn ignore_space(&self) -> bool {
        self.parser().ignore_space.get()
    }

    /// Return the character at the current position of the parser.
    ///
    /// This panics if the current position does not point to a valid char.
    fn char(&self) -> char {
        self.char_at(self.offset())
    }

    /// Return the character at the given position.
    ///
    /// This panics if the given position does not point to a valid char.
    fn char_at(&self, i: usize) -> char {
        self.pattern[i..].chars().next()
            .expect(&format!("expected char at offset {}", i))
    }

    /// Bump the parser to the next Unicode scalar value.
    ///
    /// If the end of the input has been reached, then `false` is returned.
    fn bump(&self) -> bool {
        if self.is_eof() {
            return false;
        }
        let Position { mut offset, mut line, mut column } = self.pos();
        if self.char() == '\n' {
            line = line.checked_add(1).unwrap();
            column = 1;
        } else {
            column = column.checked_add(1).unwrap();
        }
        offset += self.char().len_utf8();
        self.parser().pos.set(Position {
            offset: offset,
            line: line,
            column: column,
        });
        self.pattern[self.offset()..].chars().next().is_some()
    }

    /// If the substring starting at the current position of the parser has
    /// the given prefix, then bump the parser to the character immediately
    /// following the prefix and return true. Otherwise, don't bump the parser
    /// and return false.
    fn bump_if(&self, prefix: &str) -> bool {
        if self.pattern[self.offset()..].starts_with(prefix) {
            for _ in 0..prefix.chars().count() {
                self.bump();
            }
            true
        } else {
            false
        }
    }

    /// Returns true if and only if the parser is positioned at a look-around
    /// prefix. The conditions under which this returns true must always
    /// correspond to a regular expression that would otherwise be consider
    /// invalid.
    ///
    /// This should only be called immediately after parsing the opening of
    /// a group or a set of flags.
    fn is_lookaround_prefix(&self) -> bool {
        self.bump_if("?=")
        || self.bump_if("?!")
        || self.bump_if("?<=")
        || self.bump_if("?<!")
    }

    /// Bump the parser, and if the `x` flag is enabled, bump through any
    /// subsequent spaces. Return true if and only if the parser is not at
    /// EOF.
    fn bump_and_bump_space(&self) -> bool {
        if !self.bump() {
            return false;
        }
        self.bump_space();
        !self.is_eof()
    }

    /// If the `x` flag is enabled (i.e., whitespace insensitivity with
    /// comments), then this will advance the parser through all whitespace
    /// and comments to the next non-whitespace non-comment byte.
    ///
    /// If the `x` flag is disabled, then this is a no-op.
    ///
    /// This should be used selectively throughout the parser where
    /// arbitrary whitespace is permitted when the `x` flag is enabled. For
    /// example, `{   5  , 6}` is equivalent to `{5,6}`, but
    /// `\p{G r e e k}` is not equivalent to `\p{Greek}`.
    fn bump_space(&self) {
        if !self.ignore_space() {
            return;
        }
        while !self.is_eof() {
            if self.char().is_whitespace() {
                self.bump();
            } else if self.char() == '#' {
                let start = self.pos();
                let mut comment_text = String::new();
                self.bump();
                while !self.is_eof() {
                    let c = self.char();
                    self.bump();
                    if c == '\n' {
                        break;
                    }
                    comment_text.push(c);
                }
                let comment = ast::Comment {
                    span: Span::new(start, self.pos()),
                    comment: comment_text,
                };
                self.parser().comments.borrow_mut().push(comment);
            } else {
                break;
            }
        }
    }

    /// Peek at the next character in the input without advancing the parser.
    ///
    /// If the input has been exhausted, then this returns `None`.
    fn peek(&self) -> Option<char> {
        if self.is_eof() {
            return None;
        }
        self.pattern[self.offset() + self.char().len_utf8()..].chars().next()
    }

    /// Returns true if the next call to `bump` would return false.
    fn is_eof(&self) -> bool {
        self.offset() == self.pattern.len()
    }

    /// Return the current position of the parser, which includes the offset,
    /// line and column.
    fn pos(&self) -> Position {
        self.parser().pos.get()
    }

    /// Create a span at the current position of the parser. Both the start
    /// and end of the span are set.
    fn span(&self) -> Span {
        Span::splat(self.pos())
    }

    /// Create a span that covers the current character.
    fn span_char(&self) -> Span {
        let mut next = Position {
            offset: self.offset().checked_add(self.char().len_utf8()).unwrap(),
            line: self.line(),
            column: self.column().checked_add(1).unwrap(),
        };
        if self.char() == '\n' {
            next.line += 1;
            next.column = 1;
        }
        Span::new(self.pos(), next)
    }

    /// Parse and push a single alternation on to the parser's internal stack.
    /// If the top of the stack already has an alternation, then add to that
    /// instead of pushing a new one.
    ///
    /// The concatenation given corresponds to a single alternation branch.
    /// The concatenation returned starts the next branch and is empty.
    ///
    /// This assumes the parser is currently positioned at `|` and will advance
    /// the parser to the character following `|`.
    fn push_alternate(&self, mut concat: ast::Concat) -> Result<ast::Concat> {
        assert_eq!(self.char(), '|');
        concat.span.end = self.pos();
        self.push_or_add_alternation(concat);
        self.bump();
        Ok(ast::Concat {
            span: self.span(),
            asts: vec![],
        })
    }

    /// Pushes or adds the given branch of an alternation to the parser's
    /// internal stack of state.
    fn push_or_add_alternation(&self, concat: ast::Concat) {
        use self::GroupState::*;

        let mut stack = self.parser().stack_group.borrow_mut();
        if let Some(&mut Alternation(ref mut alts)) = stack.last_mut() {
            alts.asts.push(concat.into_ast());
            return;
        }
        stack.push(Alternation(ast::Alternation {
            span: Span::new(concat.span.start, self.pos()),
            asts: vec![concat.into_ast()],
        }));
    }

    /// Parse and push a group AST (and its parent concatenation) on to the
    /// parser's internal stack. Return a fresh concatenation corresponding
    /// to the group's sub-AST.
    ///
    /// If a set of flags was found (with no group), then the concatenation
    /// is returned with that set of flags added.
    ///
    /// This assumes that the parser is currently positioned on the opening
    /// parenthesis. It advances the parser to the character at the start
    /// of the sub-expression (or adjoining expression).
    ///
    /// If there was a problem parsing the start of the group, then an error
    /// is returned.
    fn push_group(&self, mut concat: ast::Concat) -> Result<ast::Concat> {
        assert_eq!(self.char(), '(');
        match try!(self.parse_group()) {
            Either::Left(set) => {
                let ignore = set.flags.flag_state(ast::Flag::IgnoreWhitespace);
                if let Some(v) = ignore {
                    self.parser().ignore_space.set(v);
                }

                concat.asts.push(Ast::Flags(set));
                Ok(concat)
            }
            Either::Right(group) => {
                let old_ignore_space = self.ignore_space();
                let new_ignore_space = group
                    .flags()
                    .and_then(|f| f.flag_state(ast::Flag::IgnoreWhitespace))
                    .unwrap_or(old_ignore_space);
                self.parser().stack_group.borrow_mut().push(GroupState::Group {
                    concat: concat,
                    group: group,
                    ignore_space: old_ignore_space,
                });
                self.parser().ignore_space.set(new_ignore_space);
                Ok(ast::Concat {
                    span: self.span(),
                    asts: vec![],
                })
            }
        }
    }

    /// Pop a group AST from the parser's internal stack and set the group's
    /// AST to the given concatenation. Return the concatenation containing
    /// the group.
    ///
    /// This assumes that the parser is currently positioned on the closing
    /// parenthesis and advances the parser to the character following the `)`.
    ///
    /// If no such group could be popped, then an unopened group error is
    /// returned.
    fn pop_group(&self, mut group_concat: ast::Concat) -> Result<ast::Concat> {
        use self::GroupState::*;

        assert_eq!(self.char(), ')');
        let mut stack = self.parser().stack_group.borrow_mut();
        let (mut prior_concat, mut group, ignore_space, alt) =
            match stack.pop() {
                Some(Group { concat, group, ignore_space }) => {
                    (concat, group, ignore_space, None)
                }
                Some(Alternation(alt)) => {
                    match stack.pop() {
                        Some(Group { concat, group, ignore_space }) => {
                            (concat, group, ignore_space, Some(alt))
                        }
                        None | Some(Alternation(_)) => return Err(ast::Error {
                            span: self.span_char(),
                            kind: ast::ErrorKind::GroupUnopened,
                        }),
                    }
                }
                None => return Err(ast::Error {
                    span: self.span_char(),
                    kind: ast::ErrorKind::GroupUnopened,
                }),
            };
        self.parser().ignore_space.set(ignore_space);
        group_concat.span.end = self.pos();
        self.bump();
        group.span.end = self.pos();
        match alt {
            Some(mut alt) => {
                alt.span.end = group_concat.span.end;
                alt.asts.push(group_concat.into_ast());
                group.ast = Box::new(alt.into_ast());
            }
            None => {
                group.ast = Box::new(group_concat.into_ast());
            }
        }
        prior_concat.asts.push(Ast::Group(group));
        Ok(prior_concat)
    }

    /// Pop the last state from the parser's internal stack, if it exists, and
    /// add the given concatenation to it. There either must be no state or a
    /// single alternation item on the stack. Any other scenario produces an
    /// error.
    ///
    /// This assumes that the parser has advanced to the end.
    fn pop_group_end(&self, mut concat: ast::Concat) -> Result<Ast> {
        concat.span.end = self.pos();
        let mut stack = self.parser().stack_group.borrow_mut();
        let ast = match stack.pop() {
            None => Ok(concat.into_ast()),
            Some(GroupState::Alternation(mut alt)) => {
                alt.span.end = self.pos();
                alt.asts.push(concat.into_ast());
                Ok(Ast::Alternation(alt))
            }
            Some(GroupState::Group { group, .. }) => {
                return Err(ast::Error {
                    span: group.span,
                    kind: ast::ErrorKind::GroupUnclosed,
                });
            }
        };
        // If we try to pop again, there should be nothing.
        match stack.pop() {
            None => ast,
            Some(GroupState::Alternation(_)) => {
                // This unreachable is unfortunate. This case can't happen
                // because the only way we can be here is if there were two
                // `GroupState::Alternation`s adjacent in the parser's stack,
                // which we guarantee to never happen because we never push a
                // `GroupState::Alternation` if one is already at the top of
                // the stack.
                unreachable!()
            }
            Some(GroupState::Group { group, .. }) => {
                Err(ast::Error {
                    span: group.span,
                    kind: ast::ErrorKind::GroupUnclosed,
                })
            }
        }
    }

    /// Parse the opening of a character class and push the current class
    /// parsing context onto the parser's stack. This assumes that the parser
    /// is positioned at an opening `[`. The given union should correspond to
    /// the union of set items built up before seeing the `[`.
    ///
    /// If there was a problem parsing the opening of the class, then an error
    /// is returned. Otherwise, a new union of set items for the class is
    /// returned (which may be populated with either a `]` or a `-`).
    fn push_class_open(
        &self,
        parent_union: ast::ClassSetUnion,
    ) -> Result<ast::ClassSetUnion> {
        assert_eq!(self.char(), '[');

        let (nested_set, nested_union) = try!(self.parse_set_class_open());
        self.parser().stack_class.borrow_mut().push(ClassState::Open {
            union: parent_union,
            set: nested_set,
        });
        Ok(nested_union)
    }

    /// Parse the end of a character class set and pop the character class
    /// parser stack. The union given corresponds to the last union built
    /// before seeing the closing `]`. The union returned corresponds to the
    /// parent character class set with the nested class added to it.
    ///
    /// This assumes that the parser is positioned at a `]` and will advance
    /// the parser to the byte immediately following the `]`.
    ///
    /// If the stack is empty after popping, then this returns the final
    /// "top-level" character class AST (where a "top-level" character class
    /// is one that is not nested inside any other character class).
    ///
    /// If there is no corresponding opening bracket on the parser's stack,
    /// then an error is returned.
    fn pop_class(
        &self,
        nested_union: ast::ClassSetUnion,
    ) -> Result<Either<ast::ClassSetUnion, ast::Class>> {
        assert_eq!(self.char(), ']');

        let op = self.pop_class_op(ast::ClassSetOp::Union(nested_union));
        let mut stack = self.parser().stack_class.borrow_mut();
        match stack.pop() {
            None => {
                // We can never observe an empty stack:
                //
                // 1) We are guaranteed to start with a non-empty stack since
                //    the character class parser is only initiated when it sees
                //    a `[`.
                // 2) If we ever observe an empty stack while popping after
                //    seeing a `]`, then we signal the character class parser
                //    to terminate.
                unreachable!()
            },
            Some(ClassState::Op { .. }) => {
                // This unreachable is unfortunate, but this case is impossible
                // since we already popped the Op state if one exists above.
                // Namely, every push to the class parser stack is guarded by
                // whether an existing Op is already on the top of the stack.
                // If it is, the existing Op is modified. That is, the stack
                // can never have consecutive Op states.
                unreachable!()
            }
            Some(ClassState::Open { mut union, mut set }) => {
                self.bump();
                set.span.end = self.pos();
                set.op = op;
                let class = ast::Class::Set(set);
                if stack.is_empty() {
                    Ok(Either::Right(class))
                } else {
                    union.push(ast::ClassSetItem::Class(Box::new(class)));
                    Ok(Either::Left(union))
                }
            }
        }
    }

    /// Return an "unclosed class" error whose span points to the most
    /// recently opened class.
    ///
    /// This should only be called while parsing a character class.
    fn unclosed_class_error(&self) -> ast::Error {
        for state in self.parser().stack_class.borrow().iter().rev() {
            match *state {
                ClassState::Open { ref set, .. } => {
                    return ast::Error {
                        span: set.span,
                        kind: ast::ErrorKind::ClassUnclosed,
                    };
                }
                _ => {}
            }
        }
        // We are guaranteed to have a non-empty stack with at least
        // one open bracket, so we should never get here.
        unreachable!()
    }

    /// Push the current set of class items on to the class parser's stack as
    /// the left hand side of the given operator.
    ///
    /// A fresh set union is returned, which should be used to build the right
    /// hand side of this operator.
    fn push_class_op(
        &self,
        next_kind: ast::ClassSetBinaryOpKind,
        next_union: ast::ClassSetUnion,
    ) -> ast::ClassSetUnion {

        let new_lhs = self.pop_class_op(ast::ClassSetOp::Union(next_union));
        self.parser().stack_class.borrow_mut().push(ClassState::Op {
            kind: next_kind,
            lhs: new_lhs,
        });
        ast::ClassSetUnion { span: self.span(), items: vec![] }
    }

    /// Pop a character class operation from the character class parser stack.
    /// If the top of the stack is not an operation, then return the given op
    /// unchanged. If the top of the stack is an operation, then the given
    /// op will be used as the rhs of the operation on the top of the stack.
    /// In that case, the binary operation is returned.
    fn pop_class_op(&self, rhs: ast::ClassSetOp) -> ast::ClassSetOp {
        let mut stack = self.parser().stack_class.borrow_mut();
        let (kind, lhs) = match stack.pop() {
            Some(ClassState::Op { kind, lhs }) => (kind, lhs),
            Some(state @ ClassState::Open { .. }) => {
                stack.push(state);
                return rhs;
            }
            None => unreachable!(),
        };
        let span = Span::new(lhs.span().start, rhs.span().end);
        ast::ClassSetOp::BinaryOp(ast::ClassSetBinaryOp {
            span: span,
            kind: kind,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
    }
}

impl<P: Borrow<Parser>> ParserI<P> {
    /// Parse the regular expression into an abstract syntax tree.
    fn parse(&self) -> Result<Ast> {
        self.parse_with_comments().map(|astc| astc.ast)
    }

    /// Parse the regular expression and return an abstract syntax tree with
    /// all of the comments found in the pattern.
    fn parse_with_comments(&self) -> Result<ast::WithComments> {
        assert_eq!(self.offset(), 0, "parser can only be used once");
        self.parser().reset();
        let mut concat = ast::Concat {
            span: self.span(),
            asts: vec![],
        };
        loop {
            self.bump_space();
            if self.is_eof() {
                break;
            }
            match self.char() {
                '(' => concat = try!(self.push_group(concat)),
                ')' => concat = try!(self.pop_group(concat)),
                '|' => concat = try!(self.push_alternate(concat)),
                '[' => {
                    let class = try!(self.parse_set_class());
                    concat.asts.push(Ast::Class(class));
                }
                '?' => {
                    concat = self.parse_uncounted_repetition(
                        concat, ast::RepetitionKind::ZeroOrOne);
                }
                '*' => {
                    concat = self.parse_uncounted_repetition(
                        concat, ast::RepetitionKind::ZeroOrMore);
                }
                '+' => {
                    concat = self.parse_uncounted_repetition(
                        concat, ast::RepetitionKind::OneOrMore);
                }
                '{' => {
                    concat = try!(self.parse_counted_repetition(concat));
                }
                _ => concat.asts.push(try!(self.parse_primitive()).into_ast()),
            }
        }
        let ast = try!(self.pop_group_end(concat));
        try!(error_if_nested(&ast, self.parser().nest_limit, 0));
        Ok(ast::WithComments {
            ast: ast,
            comments: mem::replace(
                &mut *self.parser().comments.borrow_mut(),
                vec![],
            ),
        })
    }

    /// Parses an uncounted repetition operation. An uncounted repetition
    /// operator includes ?, * and +, but does not include the {m,n} syntax.
    /// The given `kind` should correspond to the operator observed by the
    /// caller.
    ///
    /// This assumes that the paser is currently positioned at the repetition
    /// operator and advances the parser to the first character after the
    /// operator. (Note that the operator may include a single additional `?`,
    /// which makes the operator ungreedy.)
    ///
    /// The caller should include the concatenation that is being built. The
    /// concatenation returned includes the repetition operator applied to the
    /// last expression in the given concatenation.
    fn parse_uncounted_repetition(
        &self,
        mut concat: ast::Concat,
        kind: ast::RepetitionKind,
    ) -> ast::Concat {
        assert!(
            self.char() == '?' || self.char() == '*' || self.char() == '+');
        let op_start = self.pos();
        let ast = match concat.asts.pop() {
            None => Ast::Empty(self.span()),
            Some(ast) => ast,
        };
        let mut greedy = true;
        if self.bump() && self.char() == '?' {
            greedy = false;
            self.bump();
        }
        concat.asts.push(Ast::Repetition(ast::Repetition {
            span: ast.span().with_end(self.pos()),
            op: ast::RepetitionOp {
                span: Span::new(op_start, self.pos()),
                kind: kind,
            },
            greedy: greedy,
            ast: Box::new(ast),
        }));
        concat
    }

    /// Parses a counted repetition operation. A counted repetition operator
    /// corresponds to the {m,n} syntax, and does not include the ?, * or +
    /// operators.
    ///
    /// This assumes that the paser is currently positioned at the opening `{`
    /// and advances the parser to the first character after the operator.
    /// (Note that the operator may include a single additional `?`, which
    /// makes the operator ungreedy.)
    ///
    /// The caller should include the concatenation that is being built. The
    /// concatenation returned includes the repetition operator applied to the
    /// last expression in the given concatenation.
    fn parse_counted_repetition(
        &self,
        mut concat: ast::Concat,
    ) -> Result<ast::Concat> {
        assert!(self.char() == '{');
        let start = self.pos();
        let ast = match concat.asts.pop() {
            None => Ast::Empty(self.span()),
            Some(ast) => ast,
        };
        if !self.bump() {
            return Err(ast::Error {
                span: Span::new(start, self.pos()),
                kind: ast::ErrorKind::CountedRepetitionUnclosed,
            });
        }
        let count_start = try!(self.parse_decimal());
        let mut range = ast::RepetitionRange::Exactly(count_start);
        if self.is_eof() {
            return Err(ast::Error {
                span: Span::new(start, self.pos()),
                kind: ast::ErrorKind::CountedRepetitionUnclosed,
            });
        }
        if self.char() == ',' {
            if !self.bump() {
                return Err(ast::Error {
                    span: Span::new(start, self.pos()),
                    kind: ast::ErrorKind::CountedRepetitionUnclosed,
                });
            }
            if self.char() != '}' {
                let count_end = try!(self.parse_decimal());
                range = ast::RepetitionRange::Bounded(count_start, count_end);
            } else {
                range = ast::RepetitionRange::AtLeast(count_start);
            }
        }
        if self.is_eof() || self.char() != '}' {
            return Err(ast::Error {
                span: Span::new(start, self.pos()),
                kind: ast::ErrorKind::CountedRepetitionUnclosed,
            });
        }

        let mut greedy = true;
        if self.bump_and_bump_space() && self.char() == '?' {
            greedy = false;
            self.bump();
        }
        concat.asts.push(Ast::Repetition(ast::Repetition {
            span: ast.span().with_end(self.pos()),
            op: ast::RepetitionOp {
                span: Span::new(start, self.pos()),
                kind: ast::RepetitionKind::Range(range),
            },
            greedy: greedy,
            ast: Box::new(ast),
        }));
        Ok(concat)
    }

    /// Parse a group (which contains a sub-expression) or a set of flags.
    ///
    /// If a group was found, then it is returned with an empty AST. If a set
    /// of flags is found, then that set is returned.
    ///
    /// The parser should be positioned at the opening parenthesis.
    ///
    /// This advances the parser to the character before the start of the
    /// sub-expression (in the case of a group) or to the closing parenthesis
    /// immediately following the set of flags.
    ///
    /// # Errors
    ///
    /// If flags are given and incorrectly specified, then a corresponding
    /// error is returned.
    ///
    /// If a capture name is given and it is incorrectly specified, then a
    /// corresponding error is returned.
    fn parse_group(&self) -> Result<Either<ast::SetFlags, ast::Group>> {
        assert_eq!(self.char(), '(');
        let open_span = self.span_char();
        self.bump();
        self.bump_space();
        if self.is_lookaround_prefix() {
            return Err(ast::Error {
                span: Span::new(open_span.start, self.span().end),
                kind: ast::ErrorKind::UnsupportedLookAround,
            });
        }
        if self.bump_if("?P<") {
            let cap = try!(self.parse_capture_name());
            Ok(Either::Right(ast::Group {
                span: open_span,
                kind: ast::GroupKind::CaptureName(cap),
                ast: Box::new(Ast::Empty(self.span())),
            }))
        } else if self.bump_if("?") {
            let flags = try!(self.parse_flags());
            let char_end = self.char();
            self.bump();
            if char_end == ')' {
                Ok(Either::Left(ast::SetFlags {
                    span: Span { end: self.pos(), ..open_span },
                    flags: flags,
                }))
            } else {
                assert_eq!(char_end, ':');
                Ok(Either::Right(ast::Group {
                    span: open_span,
                    kind: ast::GroupKind::NonCapturing(flags),
                    ast: Box::new(Ast::Empty(self.span())),
                }))
            }
        } else {
            Ok(Either::Right(ast::Group {
                span: open_span,
                kind: ast::GroupKind::CaptureIndex,
                ast: Box::new(Ast::Empty(self.span())),
            }))
        }
    }

    /// Parses a capture group name. Assumes that the parser is positioned at
    /// the first character in the name following the opening `<` (and may
    /// possibly be EOF). This advances the parser to the first character
    /// following the closing `>`.
    fn parse_capture_name(&self) -> Result<ast::CaptureName> {
        if self.is_eof() {
            return Err(ast::Error {
                span: self.span(),
                kind: ast::ErrorKind::GroupNameUnexpectedEof,
            });
        }
        let start = self.pos();
        loop {
            if self.char() == '>' {
                break;
            }
            if !is_capture_char(self.char(), self.pos() == start) {
                return Err(ast::Error {
                    span: self.span_char(),
                    kind: ast::ErrorKind::GroupNameInvalid { c: self.char() },
                });
            }
            if !self.bump() {
                break;
            }
        }
        let end = self.pos();
        if self.is_eof() {
            return Err(ast::Error {
                span: self.span(),
                kind: ast::ErrorKind::GroupNameUnexpectedEof,
            });
        }
        assert_eq!(self.char(), '>');
        self.bump();
        let name = &self.pattern[start.offset..end.offset];
        if name.is_empty() {
            return Err(ast::Error {
                span: Span::new(start, start),
                kind: ast::ErrorKind::GroupNameEmpty,
            });
        }
        Ok(ast::CaptureName {
            span: Span::new(start, end),
            name: name.to_string(),
        })
    }

    /// Parse a sequence of flags starting at the current character.
    ///
    /// This advances the parser to the character immediately following the
    /// flags, which is guaranteed to be either `:` or `)`.
    ///
    /// # Errors
    ///
    /// If any flags are duplicated, then an error is returned.
    ///
    /// If the negation operator is used more than once, then an error is
    /// returned.
    ///
    /// If no flags could be found or if the negation operation is not followed
    /// by any flags, then an error is returned.
    fn parse_flags(&self) -> Result<ast::Flags> {
        let mut flags = ast::Flags {
            span: self.span(),
            items: vec![],
        };
        while self.char() != ':' && self.char() != ')' {
            if self.char() == '-' {
                let item = ast::FlagsItem {
                    span: self.span_char(),
                    kind: ast::FlagsItemKind::Negation,
                };
                if let Some(i) = flags.add_item(item) {
                    return Err(ast::Error {
                        span: self.span_char(),
                        kind: ast::ErrorKind::FlagRepeatedNegation {
                            original: flags.items[i].span,
                        },
                    });
                }
            } else {
                let item = ast::FlagsItem {
                    span: self.span_char(),
                    kind: ast::FlagsItemKind::Flag(try!(self.parse_flag())),
                };
                if let Some(i) = flags.add_item(item) {
                    return Err(ast::Error {
                        span: self.span_char(),
                        kind: ast::ErrorKind::FlagDuplicate {
                            flag: self.char(),
                            original: flags.items[i].span,
                        },
                    });
                }
            }
            if !self.bump() {
                return Err(ast::Error {
                    span: self.span(),
                    kind: ast::ErrorKind::FlagUnexpectedEof,
                });
            }
        }
        flags.span.end = self.pos();
        Ok(flags)
    }

    /// Parse the current character as a flag. Do not advance the parser.
    ///
    /// # Errors
    ///
    /// If the flag is not recognized, then an error is returned.
    fn parse_flag(&self) -> Result<ast::Flag> {
        match self.char() {
            'i' => Ok(ast::Flag::CaseInsensitive),
            'm' => Ok(ast::Flag::MultiLine),
            's' => Ok(ast::Flag::DotMatchesNewLine),
            'U' => Ok(ast::Flag::SwapGreed),
            'u' => Ok(ast::Flag::Unicode),
            'x' => Ok(ast::Flag::IgnoreWhitespace),
            c => Err(ast::Error {
                span: self.span_char(),
                kind: ast::ErrorKind::FlagUnrecognized { flag: c },
            }),
        }
    }

    /// Parse a primitive AST. e.g., A literal, non-set character class or
    /// assertion.
    ///
    /// This assumes that the parser expects a primitive at the current
    /// location. i.e., All other non-primitive cases have been handled.
    /// For example, if the parser's position is at `|`, then `|` will be
    /// treated as a literal (e.g., inside a character class).
    ///
    /// This advances the parser to the first character immediately following
    /// the primitive.
    fn parse_primitive(&self) -> Result<Primitive> {
        match self.char() {
            '\\' => self.parse_escape(),
            '.' => {
                let ast = Primitive::Dot(self.span_char());
                self.bump();
                Ok(ast)
            }
            '^' => {
                let ast = Primitive::Assertion(ast::Assertion {
                    span: self.span_char(),
                    kind: ast::AssertionKind::StartLine,
                });
                self.bump();
                Ok(ast)
            }
            '$' => {
                let ast = Primitive::Assertion(ast::Assertion {
                    span: self.span_char(),
                    kind: ast::AssertionKind::EndLine,
                });
                self.bump();
                Ok(ast)
            }
            c => {
                let ast = Primitive::Literal(ast::Literal {
                    span: self.span_char(),
                    kind: ast::LiteralKind::Verbatim,
                    c: c,
                });
                self.bump();
                Ok(ast)
            }
        }
    }

    /// Parse an escape sequence as a primitive AST.
    ///
    /// This assumes the parser is positioned at the start of the escape
    /// sequence, i.e., `\`. It advances the parser to the first position
    /// immediately following the escape sequence.
    fn parse_escape(&self) -> Result<Primitive> {
        assert_eq!(self.char(), '\\');
        let start = self.pos();
        if !self.bump() {
            return Err(ast::Error {
                span: Span::new(start, self.pos()),
                kind: ast::ErrorKind::EscapeUnexpectedEof,
            });
        }
        let c = self.char();
        // Put some of the more complicated routines into helpers.
        match c {
            '0'...'7' => {
                if !self.parser().octal {
                    return Err(ast::Error {
                        span: Span::new(start, self.span_char().end),
                        kind: ast::ErrorKind::UnsupportedBackreference,
                    });
                }
                let mut lit = self.parse_octal();
                lit.span.start = start;
                return Ok(Primitive::Literal(lit));
            }
            '8'...'9' if !self.parser().octal => {
                return Err(ast::Error {
                    span: Span::new(start, self.span_char().end),
                    kind: ast::ErrorKind::UnsupportedBackreference,
                });
            }
            'x' | 'u' | 'U' => {
                let mut lit = try!(self.parse_hex());
                lit.span.start = start;
                return Ok(Primitive::Literal(lit));
            }
            'p' | 'P' => {
                let mut cls = try!(self.parse_unicode_class());
                cls.span.start = start;
                return Ok(Primitive::Unicode(cls));
            }
            'd' | 's' | 'w' | 'D' | 'S' | 'W' => {
                let mut cls = self.parse_perl_class();
                cls.span.start = start;
                return Ok(Primitive::Perl(cls));
            }
            _ => {}
        }

        // Handle all of the one letter sequences inline.
        self.bump();
        let span = Span::new(start, self.pos());
        if is_punct(c) {
            return Ok(Primitive::Literal(ast::Literal {
                span: span,
                kind: ast::LiteralKind::Punctuation,
                c: c,
            }));
        }
        let special = |kind, c| Ok(Primitive::Literal(ast::Literal {
            span: span,
            kind: ast::LiteralKind::Special(kind),
            c: c,
        }));
        match c {
            'a' => special(ast::SpecialLiteralKind::Bell, '\x07'),
            'f' => special(ast::SpecialLiteralKind::FormFeed, '\x0C'),
            't' => special(ast::SpecialLiteralKind::Tab, '\t'),
            'n' => special(ast::SpecialLiteralKind::LineFeed, '\n'),
            'r' => special(ast::SpecialLiteralKind::CarriageReturn, '\r'),
            'v' => special(ast::SpecialLiteralKind::VerticalTab, '\x0B'),
            ' ' if self.ignore_space() => {
                special(ast::SpecialLiteralKind::Space, ' ')
            }
            'A' => Ok(Primitive::Assertion(ast::Assertion {
                span: span,
                kind: ast::AssertionKind::StartText,
            })),
            'z' => Ok(Primitive::Assertion(ast::Assertion {
                span: span,
                kind: ast::AssertionKind::EndText,
            })),
            'b' => Ok(Primitive::Assertion(ast::Assertion {
                span: span,
                kind: ast::AssertionKind::WordBoundary,
            })),
            'B' => Ok(Primitive::Assertion(ast::Assertion {
                span: span,
                kind: ast::AssertionKind::NotWordBoundary,
            })),
            c => Err(ast::Error {
                span: span,
                kind: ast::ErrorKind::EscapeUnrecognized { c: c },
            }),
        }
    }

    /// Parse an octal representation of a Unicode codepoint up to 3 digits
    /// long. This expects the parser to be positioned at the first octal
    /// digit and advances the parser to the first character immediately
    /// following the octal number. This also assumes that parsing octal
    /// escapes is enabled.
    ///
    /// Assuming the preconditions are met, this routine can never fail.
    fn parse_octal(&self) -> ast::Literal {
        use std::char;
        use std::u32;

        assert!(self.parser().octal);
        assert!('0' <= self.char() && self.char() <= '7');
        let start = self.pos();
        // Parse up to two more digits.
        while
            self.bump() &&
            '0' <= self.char() && self.char() <= '7' &&
            self.pos().offset - start.offset <= 2
        {}
        let end = self.pos();
        let octal = &self.pattern[start.offset..end.offset];
        // Parsing the octal should never fail since the above guarantees a
        // valid number.
        let codepoint =
            u32::from_str_radix(octal, 8).expect("valid octal number");
        // The max value for 3 digit octal is 0777 = 511 and [0, 511] has no
        // invalid Unicode scalar values.
        let c = char::from_u32(codepoint).expect("Unicode scalar value");
        ast::Literal {
            span: Span::new(start, end),
            kind: ast::LiteralKind::Octal,
            c: c,
        }
    }

    /// Parse a hex representation of a Unicode codepoint. This handles both
    /// hex notations, i.e., `\xFF` and `\x{FFFF}`. This expects the parser to
    /// be positioned at the `x`, `u` or `U` prefix. The parser is advanced to
    /// the first character immediately following the hexadecimal literal.
    fn parse_hex(&self) -> Result<ast::Literal> {
        assert!(self.char() == 'x'
                || self.char() == 'u'
                || self.char() == 'U');

        let hex_kind = match self.char() {
            'x' => ast::HexLiteralKind::X,
            'u' => ast::HexLiteralKind::UnicodeShort,
            _ => ast::HexLiteralKind::UnicodeLong,
        };
        if !self.bump() {
            return Err(ast::Error {
                span: self.span(),
                kind: ast::ErrorKind::EscapeUnexpectedEof,
            });
        }
        if self.char() == '{' {
            self.parse_hex_brace(hex_kind)
        } else {
            self.parse_hex_digits(hex_kind)
        }
    }

    /// Parse an N-digit hex representation of a Unicode codepoint. This
    /// expects the parser to be positioned at the first digit and will advance
    /// the parser to the first character immediately following the escape
    /// sequence.
    ///
    /// The number of digits given must be 2 (for `\xNN`), 4 (for `\uNNNN`)
    /// or 8 (for `\UNNNNNNNN`).
    fn parse_hex_digits(&self, kind: ast::HexLiteralKind) -> Result<ast::Literal> {
        use std::char;
        use std::u32;

        let start = self.pos();
        for i in 0..kind.digits() {
            if i > 0 && !self.bump() {
                return Err(ast::Error {
                    span: self.span(),
                    kind: ast::ErrorKind::EscapeUnexpectedEof,
                });
            }
            if !is_hex(self.char()) {
                return Err(ast::Error {
                    span: self.span_char(),
                    kind: ast::ErrorKind::EscapeHexInvalidDigit {
                        c: self.char(),
                    },
                });
            }
        }
        // The final bump just moves the parser past the literal, which may
        // be EOF.
        self.bump();
        let end = self.pos();
        let hex = &self.pattern[start.offset..end.offset];
        match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
            None => Err(ast::Error {
                span: Span::new(start, end),
                kind: ast::ErrorKind::EscapeHexInvalid,
            }),
            Some(c) => Ok(ast::Literal {
                span: Span::new(start, end),
                kind: ast::LiteralKind::HexFixed(kind),
                c: c,
            }),
        }
    }

    /// Parse a hex representation of any Unicode scalar value. This expects
    /// the parser to be positioned at the opening brace `{` and will advance
    /// the parser to the first character following the closing brace `}`.
    fn parse_hex_brace(&self, kind: ast::HexLiteralKind) -> Result<ast::Literal> {
        use std::char;
        use std::u32;

        let brace_pos = self.pos();
        let start = self.span_char().end;
        while self.bump() && self.char() != '}' {
            if !is_hex(self.char()) {
                return Err(ast::Error {
                    span: self.span_char(),
                    kind: ast::ErrorKind::EscapeHexInvalidDigit {
                        c: self.char(),
                    },
                });
            }
        }
        if self.is_eof() {
            return Err(ast::Error {
                span: Span::new(brace_pos, self.pos()),
                kind: ast::ErrorKind::EscapeUnexpectedEof,
            })
        }
        let end = self.pos();
        let hex = &self.pattern[start.offset..end.offset];
        assert_eq!(self.char(), '}');
        self.bump();

        if hex.is_empty() {
            return Err(ast::Error {
                span: Span::new(brace_pos, self.pos()),
                kind: ast::ErrorKind::EscapeHexEmpty,
            })
        }
        match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
            None => Err(ast::Error {
                span: Span::new(start, end),
                kind: ast::ErrorKind::EscapeHexInvalid,
            }),
            Some(c) => Ok(ast::Literal {
                span: Span::new(start, self.pos()),
                kind: ast::LiteralKind::HexBrace(kind),
                c: c,
            }),
        }
    }

    /// Parse a decimal number into a u32 while trimming leading and trailing
    /// whitespace.
    ///
    /// This expects the parser to be positioned at the first position where
    /// a decimal digit could occur. This will advance the parser to the byte
    /// immediately following the last contiguous decimal digit.
    ///
    /// If no decimal digit could be found or if there was a problem parsing
    /// the complete set of digits into a u32, then an error is returned.
    fn parse_decimal(&self) -> Result<u32> {
        while !self.is_eof() && self.char().is_whitespace() {
            self.bump();
        }
        let start = self.pos();
        while !self.is_eof() && '0' <= self.char() && self.char() <= '9' {
            self.bump();
        }
        let span = Span::new(start, self.pos());
        while !self.is_eof() && self.char().is_whitespace() {
            self.bump();
        }
        let digits = &self.pattern[span.start.offset..span.end.offset];
        if digits.is_empty() {
            return Err(ast::Error {
                span: span,
                kind: ast::ErrorKind::DecimalEmpty,
            });
        }
        match u32::from_str_radix(digits, 10).ok() {
            Some(n) => Ok(n),
            None => Err(ast::Error {
                span: span,
                kind: ast::ErrorKind::DecimalInvalid,
            }),
        }
    }

    /// Parse a standard character class consisting primarily of characters or
    /// character ranges, but can also contain nested character classes of
    /// any type (sans `.`).
    ///
    /// This assumes the parser is positioned at the opening `[`. If parsing
    /// is successful, then the parser is advanced to the position immediately
    /// following the closing `]`.
    fn parse_set_class(&self) -> Result<ast::Class> {
        assert_eq!(self.char(), '[');

        let mut union = ast::ClassSetUnion { span: self.span(), items: vec![] };
        loop {
            self.bump_space();
            if self.is_eof() {
                return Err(self.unclosed_class_error());
            }
            match self.char() {
                '[' => {
                    // If we've already parsed the opening bracket, then
                    // attempt to treat this as the beginning of an ASCII
                    // class. If ASCII class parsing fails, then the parser
                    // backs up to `[`.
                    if !self.parser().stack_class.borrow().is_empty() {
                        if let Some(cls) = self.maybe_parse_ascii_class() {
                            union.push(ast::ClassSetItem::Ascii(cls));
                            continue;
                        }
                    }
                    union = try!(self.push_class_open(union));
                }
                ']' => {
                    match try!(self.pop_class(union)) {
                        Either::Left(nested_union) => { union = nested_union; }
                        Either::Right(class) => return Ok(class),
                    }
                }
                '&' if self.peek() == Some('&') => {
                    assert!(self.bump_if("&&"));
                    union = self.push_class_op(
                        ast::ClassSetBinaryOpKind::Intersection, union);
                }
                '-' if self.peek() == Some('-') => {
                    assert!(self.bump_if("--"));
                    union = self.push_class_op(
                        ast::ClassSetBinaryOpKind::Difference, union);
                }
                '~' if self.peek() == Some('~') => {
                    assert!(self.bump_if("~~"));
                    union = self.push_class_op(
                        ast::ClassSetBinaryOpKind::SymmetricDifference, union);
                }
                _ => {
                    union.push(try!(self.parse_set_class_range()));
                }
            }
        }
    }

    /// Parse a single primitive item in a character class set. The item to
    /// be parsed can either be one of a simple literal character, a range
    /// between two simple literal characters or a "primitive" character
    /// class like \w or \p{Greek}.
    ///
    /// If an invalid escape is found, or if a character class is found where
    /// a simple literal is expected (e.g., in a range), then an error is
    /// returned.
    fn parse_set_class_range(&self) -> Result<ast::ClassSetItem> {
        let prim1 = try!(self.parse_set_class_item());
        self.bump_space();
        if self.is_eof() {
            return Err(self.unclosed_class_error());
        }
        // If the next char isn't a `-`, then we don't have a range.
        // There are two exceptions. If the char after a `-` is a `]`, then
        // `-` is interpreted as a literal `-`. Alternatively, if the char
        // after a `-` is a `-`, then `--` corresponds to a "difference"
        // operation.
        if self.char() != '-'
            || self.peek() == Some(']')
            || self.peek() == Some('-')
        {
            return prim1.into_class_set_item();
        }
        // OK, now we're parsing a range, so bump past the `-` and parse the
        // second half of the range.
        if !self.bump_and_bump_space() {
            return Err(self.unclosed_class_error());
        }
        let prim2 = try!(self.parse_set_class_item());
        Ok(ast::ClassSetItem::Range(ast::ClassSetRange {
            span: Span::new(prim1.span().start, prim2.span().end),
            start: try!(prim1.into_class_literal()),
            end: try!(prim2.into_class_literal()),
        }))
    }

    /// Parse a single item in a character class as a primitive, where the
    /// primitive either consists of a verbatim literal or a single escape
    /// sequence.
    ///
    /// This assumes the parser is positioned at the beginning of a primitive,
    /// and advances the parser to the first position after the primitive if
    /// successful.
    ///
    /// Note that it is the caller's responsibility to report an error if an
    /// illegal primitive was parsed.
    fn parse_set_class_item(&self) -> Result<Primitive> {
        if self.char() == '\\' {
            self.parse_escape()
        } else {
            let x = Primitive::Literal(ast::Literal {
                span: self.span_char(),
                kind: ast::LiteralKind::Verbatim,
                c: self.char(),
            });
            self.bump();
            Ok(x)
        }
    }

    /// Parses the opening of a character class set. This includes the opening
    /// bracket along with `^` if present to indicate negation. This also
    /// starts parsing the opening set of unioned items if applicable, since
    /// there are special rules applied to certain characters in the opening
    /// of a character class. For example, `[^]]` is the class of all
    /// characters not equal to `]`. (`]` would need to be escaped in any other
    /// position.) Similarly for `-`.
    ///
    /// In all cases, the op inside the returned `ast::ClassSet` is an empty
    /// union. This empty union should be replaced with the actual op when
    /// it is popped from the parser's stack.
    ///
    /// This assumes the parser is positioned at the opening `[` and advances
    /// the parser to the first non-special byte of the character class.
    ///
    /// An error is returned if EOF is found.
    fn parse_set_class_open(&self) -> Result<(ast::ClassSet, ast::ClassSetUnion)> {
        assert_eq!(self.char(), '[');
        let start = self.pos();
        if !self.bump_and_bump_space() {
            return Err(ast::Error {
                span: Span::new(start, self.pos()),
                kind: ast::ErrorKind::ClassUnclosed,
            });
        }

        let negated =
            if self.char() != '^' {
                false
            } else {
                if !self.bump_and_bump_space() {
                    return Err(ast::Error {
                        span: Span::new(start, self.pos()),
                        kind: ast::ErrorKind::ClassUnclosed,
                    });
                }
                true
            };
        // Accept any number of `-` as literal `-`.
        let mut union = ast::ClassSetUnion { span: self.span(), items: vec![] };
        while self.char() == '-' {
            union.push(ast::ClassSetItem::Literal(ast::Literal {
                span: self.span_char(),
                kind: ast::LiteralKind::Verbatim,
                c: '-',
            }));
            if !self.bump_and_bump_space() {
                return Err(ast::Error {
                    span: Span::new(start, self.pos()),
                    kind: ast::ErrorKind::ClassUnclosed,
                });
            }
        }
        // If `]` is the *first* char in a set, then interpret it as a literal
        // `]`. That is, an empty class is impossible to write.
        if union.items.is_empty() && self.char() == ']' {
            union.push(ast::ClassSetItem::Literal(ast::Literal {
                span: self.span_char(),
                kind: ast::LiteralKind::Verbatim,
                c: ']',
            }));
            if !self.bump_and_bump_space() {
                return Err(ast::Error {
                    span: Span::new(start, self.pos()),
                    kind: ast::ErrorKind::ClassUnclosed,
                });
            }
        }
        let set = ast::ClassSet {
            span: Span::new(start, self.pos()),
            negated: negated,
            op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                span: Span::new(union.span.start, union.span.start),
                items: vec![],
            }),
        };
        Ok((set, union))
    }

    /// Attempt to parse an ASCII character class, e.g., `[:alnum:]`.
    ///
    /// This assumes the parser is positioned at the opening `[`.
    ///
    /// If no valid ASCII character class could be found, then this does not
    /// advance the parser and `None` is returned. Otherwise, the parser is
    /// advanced to the first byte following the closing `]` and the
    /// corresponding ASCII class is returned.
    fn maybe_parse_ascii_class(&self) -> Option<ast::ClassAscii> {
        // ASCII character classes are interesting from a parsing perspective
        // because parsing cannot fail with any interesting error. For example,
        // in order to use an ASCII character class, it must be enclosed in
        // double brackets, e.g., `[[:alnum:]]`. Alternatively, you might think
        // of it as "ASCII character characters have the syntax `[:NAME:]`
        // which can only appear within character brackets." This means that
        // things like `[[:lower:]A]` are legal constructs.
        //
        // However, if one types an incorrect ASCII character class, e.g.,
        // `[[:loower:]]`, then we treat that as a normal nested character
        // class containing the characters `:elorw`. One might argue that we
        // should return an error instead since the repeated colons give away
        // the intent to write an ASCII class. But what if the user typed
        // `[[:lower]]` instead? How can we tell that was intended to be an
        // ASCII class and not just a normal nested class?
        //
        // Reasonable people can probably disagree over this, but for better
        // or worse, we implement semantics that never fails at the expense
        // of better failure modes.
        assert_eq!(self.char(), '[');
        // If parsing fails, then we back up the parser to this starting point.
        let start = self.pos();
        let mut negated = false;
        if !self.bump() || self.char() != ':' {
            self.parser().pos.set(start);
            return None;
        }
        if !self.bump() {
            self.parser().pos.set(start);
            return None;
        }
        if self.char() == '^' {
            negated = true;
            if !self.bump() {
                self.parser().pos.set(start);
                return None;
            }
        }
        let name_start = self.offset();
        while self.char() != ':' && self.bump() {}
        if self.is_eof() {
            self.parser().pos.set(start);
            return None;
        }
        let name = &self.pattern[name_start..self.offset()];
        if !self.bump_if(":]") {
            self.parser().pos.set(start);
            return None;
        }
        let kind = match ast::ClassAsciiKind::from_name(name) {
            Some(kind) => kind,
            None => {
                self.parser().pos.set(start);
                return None;
            }
        };
        Some(ast::ClassAscii {
            span: Span::new(start, self.pos()),
            kind: kind,
            negated: negated,
        })
    }

    /// Parse a Unicode class in either the single character notation, `\pN`
    /// or the multi-character bracketed notation, `\p{Greek}`. This assumes
    /// the parser is positioned at the `p` (or `P` for negation) and will
    /// advance the parser to the character immediately following the class.
    ///
    /// Note that this does not check whether the class name is valid or not.
    fn parse_unicode_class(&self) -> Result<ast::ClassUnicode> {
        assert!(self.char() == 'p' || self.char() == 'P');
        let negated = self.char() == 'P';
        if !self.bump() {
            return Err(ast::Error {
                span: self.span(),
                kind: ast::ErrorKind::EscapeUnexpectedEof,
            });
        }
        let (start, kind) =
            if self.char() == '{' {
                let start = self.span_char().end;
                while self.bump() && self.char() != '}' {}
                if self.is_eof() {
                    return Err(ast::Error {
                        span: self.span(),
                        kind: ast::ErrorKind::EscapeUnexpectedEof,
                    });
                }
                assert_eq!(self.char(), '}');
                let end = self.pos();
                self.bump();

                let name = &self.pattern[start.offset..end.offset];
                if let Some(i) = name.find("!=") {
                    (start, ast::ClassUnicodeKind::NamedValue {
                        op: ast::ClassUnicodeOpKind::NotEqual,
                        name: name[..i].to_string(),
                        value: name[i+2..].to_string(),
                    })
                } else if let Some(i) = name.find(':') {
                    (start, ast::ClassUnicodeKind::NamedValue {
                        op: ast::ClassUnicodeOpKind::Colon,
                        name: name[..i].to_string(), value: name[i+1..].to_string(),
                    })
                } else if let Some(i) = name.find('=') {
                    (start, ast::ClassUnicodeKind::NamedValue {
                        op: ast::ClassUnicodeOpKind::Equal,
                        name: name[..i].to_string(),
                        value: name[i+1..].to_string(),
                    })
                } else {
                    (start, ast::ClassUnicodeKind::Named(name.to_string()))
                }
            } else {
                let start = self.pos();
                let c = self.char();
                self.bump();
                let kind = ast::ClassUnicodeKind::OneLetter(c);
                (start, kind)
            };
        Ok(ast::ClassUnicode {
            span: Span::new(start, self.pos()),
            negated: negated,
            kind: kind,
        })
    }

    /// Parse a Perl character class, e.g., `\d` or `\W`. This assumes the
    /// parser is currently at a valid character class name and will be
    /// advanced to the character immediately following the class.
    fn parse_perl_class(&self) -> ast::ClassPerl {
        let c = self.char();
        let span = self.span_char();
        self.bump();
        let (negated, kind) = match c {
            'd' => (false, ast::ClassPerlKind::Digit),
            'D' => (true, ast::ClassPerlKind::Digit),
            's' => (false, ast::ClassPerlKind::Space),
            'S' => (true, ast::ClassPerlKind::Space),
            'w' => (false, ast::ClassPerlKind::Word),
            'W' => (true, ast::ClassPerlKind::Word),
            c => panic!("expected valid Perl class but got '{}'", c),
        };
        ast::ClassPerl { span: span, kind: kind, negated: negated }
    }
}

/// Returns an error if the given AST exceeds the given depth limit.
fn error_if_nested(
    ast: &Ast,
    limit: u32,
    depth: u32,
) -> Result<()> {
    if depth >= limit {
        return Err(ast::Error {
            span: *ast.span(),
            kind: ast::ErrorKind::NestLimitExceeded(limit),
        });
    }
    match *ast {
        Ast::Empty(_)
        | Ast::Flags(_)
        | Ast::Literal(_)
        | Ast::Dot(_)
        | Ast::Assertion(_) => {
            Ok(())
        }
        Ast::Class(ref cls) => {
            error_if_nested_class(cls, limit, depth)
        }
        Ast::Repetition(ast::Repetition { ref ast, .. }) => {
            error_if_nested(ast, limit, depth.checked_add(1).unwrap())
        }
        Ast::Group(ast::Group { ref ast, .. }) => {
            error_if_nested(ast, limit, depth.checked_add(1).unwrap())
        }
        Ast::Alternation(ast::Alternation { ref asts, .. }) => {
            let depth = depth.checked_add(1).unwrap();
            for ast in asts {
                try!(error_if_nested(ast, limit, depth));
            }
            Ok(())
        }
        Ast::Concat(ast::Concat { ref asts, .. }) => {
            let depth = depth.checked_add(1).unwrap();
            for ast in asts {
                try!(error_if_nested(ast, limit, depth));
            }
            Ok(())
        }
    }
}

/// Returns an error if the given AST class exceeds the given depth limit.
fn error_if_nested_class(
    class: &ast::Class,
    limit: u32,
    depth: u32,
) -> Result<()> {
    if depth >= limit {
        return Err(ast::Error {
            span: *class.span(),
            kind: ast::ErrorKind::NestLimitExceeded(limit),
        });
    }
    match *class {
        ast::Class::Perl(_)
        | ast::Class::Unicode(_) => Ok(()),
        ast::Class::Set(ast::ClassSet { ref op, .. }) => {
            error_if_nested_class_op(op, limit, depth)
        }
    }
}

fn error_if_nested_class_op(
    op: &ast::ClassSetOp,
    limit: u32,
    depth: u32,
) -> Result<()> {
    if depth >= limit {
        return Err(ast::Error {
            span: *op.span(),
            kind: ast::ErrorKind::NestLimitExceeded(limit),
        });
    }
    match *op {
        ast::ClassSetOp::Union(ast::ClassSetUnion { ref items, .. }) => {
            for item in items {
                match *item {
                    ast::ClassSetItem::Literal(_)
                    | ast::ClassSetItem::Range(_)
                    | ast::ClassSetItem::Ascii(_) => {}
                    ast::ClassSetItem::Class(ref cls) => {
                        let depth = depth.checked_add(1).unwrap();
                        try!(error_if_nested_class(cls, limit, depth));
                    }
                }
            }
            Ok(())
        }
        ast::ClassSetOp::BinaryOp(ast::ClassSetBinaryOp {
            ref lhs, ref rhs, ..
        }) => {
            let depth = depth.checked_add(1).unwrap();
            try!(error_if_nested_class_op(lhs, limit, depth));
            try!(error_if_nested_class_op(rhs, limit, depth));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use ast::{self, Ast, Position, Span};
    use super::{Parser, ParserI, ParserBuilder, Primitive};

    macro_rules! assert_eq {
        ($left:expr, $right:expr) => ({
            match (&$left, &$right) {
                (left_val, right_val) => {
                    if !(*left_val == *right_val) {
                        panic!("assertion failed: `(left == right)`\n\n\
                               left:  `{:?}`\nright: `{:?}`\n\n",
                               left_val, right_val)
                    }
                }
            }
        });
    }

    fn s(str: &str) -> String {
        str.to_string()
    }

    fn parser(pattern: &str) -> ParserI<Parser> {
        ParserI::new(Parser::new(), pattern)
    }

    fn parser_octal(pattern: &str) -> ParserI<Parser> {
        ParserI::new(ParserBuilder::new().octal(true).build(), pattern)
    }

    fn parser_nest_limit(pattern: &str, nest_limit: u32) -> ParserI<Parser> {
        let p = ParserBuilder::new().nest_limit(nest_limit).build();
        ParserI::new(p, pattern)
    }

    fn parser_ignore_space(pattern: &str) -> ParserI<Parser> {
        let p = ParserBuilder::new().ignore_space(true).build();
        ParserI::new(p, pattern)
    }

    /// Short alias for creating a new span.
    fn nspan(start: Position, end: Position) -> Span {
        Span::new(start, end)
    }

    /// Short alias for creating a new position.
    fn npos(offset: usize, line: usize, column: usize) -> Position {
        Position::new(offset, line, column)
    }

    /// Create a new span from the given offset range. This assumes a single
    /// line and sets the columns based on the offsets. i.e., This only works
    /// out of the box for ASCII, which is fine for most tests.
    fn span(range: Range<usize>) -> Span {
        let start = Position::new(range.start, 1, range.start + 1);
        let end = Position::new(range.end, 1, range.end + 1);
        Span::new(start, end)
    }

    /// Create a new span for the corresponding byte range in the given string.
    fn span_range(subject: &str, range: Range<usize>) -> Span {
        let start = Position {
            offset: range.start,
            line: 1 + subject[..range.start].matches('\n').count(),
            column: 1 + subject[..range.start]
                .chars()
                .rev()
                .position(|c| c == '\n')
                .unwrap_or(subject[..range.start].chars().count()),
        };
        let end = Position {
            offset: range.end,
            line: 1 + subject[..range.end].matches('\n').count(),
            column: 1 + subject[..range.end]
                .chars()
                .rev()
                .position(|c| c == '\n')
                .unwrap_or(subject[..range.end].chars().count()),
        };
        Span::new(start, end)
    }

    /// Create a verbatim literal starting at the given position.
    fn lit(c: char, start: usize) -> Ast {
        lit_with(c, span(start..start + c.len_utf8()))
    }

    /// Create a punctuation literal starting at the given position.
    fn punct_lit(c: char, span: Span) -> Ast {
        Ast::Literal(ast::Literal {
            span: span,
            kind: ast::LiteralKind::Punctuation,
            c: c,
        })
    }

    /// Create a verbatim literal with the given span.
    fn lit_with(c: char, span: Span) -> Ast {
        Ast::Literal(ast::Literal {
            span: span,
            kind: ast::LiteralKind::Verbatim,
            c: c,
        })
    }

    /// Create a concatenation with the given range.
    fn concat(range: Range<usize>, asts: Vec<Ast>) -> Ast {
        concat_with(span(range), asts)
    }

    /// Create a concatenation with the given span.
    fn concat_with(span: Span, asts: Vec<Ast>) -> Ast {
        Ast::Concat(ast::Concat { span: span, asts: asts })
    }

    /// Create an alternation with the given span.
    fn alt(range: Range<usize>, asts: Vec<Ast>) -> Ast {
        Ast::Alternation(ast::Alternation { span: span(range), asts: asts })
    }

    /// Create a capturing group with the given span.
    fn group(range: Range<usize>, ast: Ast) -> Ast {
        Ast::Group(ast::Group {
            span: span(range),
            kind: ast::GroupKind::CaptureIndex,
            ast: Box::new(ast),
        })
    }

    /// Create an ast::SetFlags.
    ///
    /// The given pattern should be the full pattern string. The range given
    /// should correspond to the byte offsets where the flag set occurs.
    ///
    /// If negated is true, then the set is interpreted as beginning with a
    /// negation.
    fn flag_set(
        pat: &str,
        range: Range<usize>,
        flag: ast::Flag,
        negated: bool,
    ) -> Ast {
        let mut items = vec![
            ast::FlagsItem {
                span: span_range(pat, (range.end - 2)..(range.end - 1)),
                kind: ast::FlagsItemKind::Flag(flag),
            },
        ];
        if negated {
            items.insert(0, ast::FlagsItem {
                span: span_range(pat, (range.start + 2)..(range.end - 2)),
                kind: ast::FlagsItemKind::Negation,
            });
        }
        Ast::Flags(ast::SetFlags {
            span: span_range(pat, range.clone()),
            flags: ast::Flags {
                span: span_range(pat, (range.start + 2)..(range.end - 1)),
                items: items,
            },
        })
    }

    #[test]
    fn parse_nest_limit() {
        assert_eq!(
            parser_nest_limit("", 0).parse(),
            Err(ast::Error {
                span: span(0..0),
                kind: ast::ErrorKind::NestLimitExceeded(0),
            }));
        assert_eq!(
            parser_nest_limit("", 1).parse(),
            Ok(Ast::Empty(span(0..0))));
        assert_eq!(
            parser_nest_limit("a", 0).parse(),
            Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::NestLimitExceeded(0),
            }));
        assert_eq!(
            parser_nest_limit("a", 1).parse(),
            Ok(lit('a', 0)));
        assert_eq!(
            parser_nest_limit("((()))", 0).parse(),
            Err(ast::Error {
                span: span(0..6),
                kind: ast::ErrorKind::NestLimitExceeded(0),
            }));
        assert_eq!(
            parser_nest_limit("((()))", 1).parse(),
            Err(ast::Error {
                span: span(1..5),
                kind: ast::ErrorKind::NestLimitExceeded(1),
            }));
        assert_eq!(
            parser_nest_limit("((()))", 2).parse(),
            Err(ast::Error {
                span: span(2..4),
                kind: ast::ErrorKind::NestLimitExceeded(2),
            }));
        assert_eq!(
            parser_nest_limit("((()))", 3).parse(),
            Err(ast::Error {
                span: span(3..3),
                kind: ast::ErrorKind::NestLimitExceeded(3),
            }));
        assert_eq!(
            parser_nest_limit("ab+", 2).parse(),
            Err(ast::Error {
                span: span(1..2),
                kind: ast::ErrorKind::NestLimitExceeded(2),
            }));
        assert_eq!(
            parser_nest_limit("[ab[cd]]", 1).parse(),
            Err(ast::Error {
                span: span(3..7),
                kind: ast::ErrorKind::NestLimitExceeded(1),
            }));
        assert_eq!(
            parser_nest_limit("[ab--cd]", 1).parse(),
            Err(ast::Error {
                span: span(1..3),
                kind: ast::ErrorKind::NestLimitExceeded(1),
            }));
    }

    #[test]
    fn parse_comments() {
        let pat = "(?x)
# This is comment 1.
foo # This is comment 2.
  # This is comment 3.
bar
# This is comment 4.";
        let astc = parser(pat).parse_with_comments().unwrap();
        assert_eq!(
            astc.ast,
            concat_with(span_range(pat, 0..pat.len()), vec![
                flag_set(pat, 0..4, ast::Flag::IgnoreWhitespace, false),
                lit_with('f', span_range(pat, 26..27)),
                lit_with('o', span_range(pat, 27..28)),
                lit_with('o', span_range(pat, 28..29)),
                lit_with('b', span_range(pat, 74..75)),
                lit_with('a', span_range(pat, 75..76)),
                lit_with('r', span_range(pat, 76..77)),
            ]));
        assert_eq!(astc.comments, vec![
            ast::Comment {
                span: span_range(pat, 5..26),
                comment: s(" This is comment 1."),
            },
            ast::Comment {
                span: span_range(pat, 30..51),
                comment: s(" This is comment 2."),
            },
            ast::Comment {
                span: span_range(pat, 53..74),
                comment: s(" This is comment 3."),
            },
            ast::Comment {
                span: span_range(pat, 78..98),
                comment: s(" This is comment 4."),
            },
        ]);
    }

    #[test]
    fn parse_holistic() {
        assert_eq!(
            parser("]").parse(),
            Ok(lit(']', 0)));
        assert_eq!(
            parser(r"\\\.\+\*\?\(\)\|\[\]\{\}\^\$\#\&\-\~").parse(),
            Ok(concat(0..36, vec![
                punct_lit('\\', span(0..2)),
                punct_lit('.', span(2..4)),
                punct_lit('+', span(4..6)),
                punct_lit('*', span(6..8)),
                punct_lit('?', span(8..10)),
                punct_lit('(', span(10..12)),
                punct_lit(')', span(12..14)),
                punct_lit('|', span(14..16)),
                punct_lit('[', span(16..18)),
                punct_lit(']', span(18..20)),
                punct_lit('{', span(20..22)),
                punct_lit('}', span(22..24)),
                punct_lit('^', span(24..26)),
                punct_lit('$', span(26..28)),
                punct_lit('#', span(28..30)),
                punct_lit('&', span(30..32)),
                punct_lit('-', span(32..34)),
                punct_lit('~', span(34..36)),
            ])));
    }

    #[test]
    fn parse_ignore_space() {
        // Test that basic whitespace insensitivity works.
        let pat = "(?x)a b";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(nspan(npos(0, 1, 1), npos(7, 1, 8)), vec![
                flag_set(pat, 0..4, ast::Flag::IgnoreWhitespace, false),
                lit_with('a', nspan(npos(4, 1, 5), npos(5, 1, 6))),
                lit_with('b', nspan(npos(6, 1, 7), npos(7, 1, 8))),
            ])));

        // Test that we can toggle whitespace insensitivity.
        let pat = "(?x)a b(?-x)a b";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(nspan(npos(0, 1, 1), npos(15, 1, 16)), vec![
                flag_set(pat, 0..4, ast::Flag::IgnoreWhitespace, false),
                lit_with('a', nspan(npos(4, 1, 5), npos(5, 1, 6))),
                lit_with('b', nspan(npos(6, 1, 7), npos(7, 1, 8))),
                flag_set(pat, 7..12, ast::Flag::IgnoreWhitespace, true),
                lit_with('a', nspan(npos(12, 1, 13), npos(13, 1, 14))),
                lit_with(' ', nspan(npos(13, 1, 14), npos(14, 1, 15))),
                lit_with('b', nspan(npos(14, 1, 15), npos(15, 1, 16))),
            ])));

        // Test that nesting whitespace insensitive flags works.
        let pat = "a (?x:a )a ";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(span_range(pat, 0..11), vec![
                lit_with('a', span_range(pat, 0..1)),
                lit_with(' ', span_range(pat, 1..2)),
                Ast::Group(ast::Group {
                    span: span_range(pat, 2..9),
                    kind: ast::GroupKind::NonCapturing(ast::Flags {
                        span: span_range(pat, 4..5),
                        items: vec![
                            ast::FlagsItem {
                                span: span_range(pat, 4..5),
                                kind: ast::FlagsItemKind::Flag(
                                    ast::Flag::IgnoreWhitespace),
                            },
                        ],
                    }),
                    ast: Box::new(lit_with('a', span_range(pat, 6..7))),
                }),
                lit_with('a', span_range(pat, 9..10)),
                lit_with(' ', span_range(pat, 10..11)),
            ])));

        // Test that whitespace after an opening paren is insignificant.
        let pat = "(?x)( ?P<foo> a )";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(span_range(pat, 0..pat.len()), vec![
                flag_set(pat, 0..4, ast::Flag::IgnoreWhitespace, false),
                Ast::Group(ast::Group {
                    span: span_range(pat, 4..pat.len()),
                    kind: ast::GroupKind::CaptureName(ast::CaptureName {
                        span: span_range(pat, 9..12),
                        name: s("foo"),
                    }),
                    ast: Box::new(lit_with('a', span_range(pat, 14..15))),
                }),
            ])));
        let pat = "(?x)(  a )";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(span_range(pat, 0..pat.len()), vec![
                flag_set(pat, 0..4, ast::Flag::IgnoreWhitespace, false),
                Ast::Group(ast::Group {
                    span: span_range(pat, 4..pat.len()),
                    kind: ast::GroupKind::CaptureIndex,
                    ast: Box::new(lit_with('a', span_range(pat, 7..8))),
                }),
            ])));
        let pat = "(?x)(  ?:  a )";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(span_range(pat, 0..pat.len()), vec![
                flag_set(pat, 0..4, ast::Flag::IgnoreWhitespace, false),
                Ast::Group(ast::Group {
                    span: span_range(pat, 4..pat.len()),
                    kind: ast::GroupKind::NonCapturing(ast::Flags {
                        span: span_range(pat, 8..8),
                        items: vec![],
                    }),
                    ast: Box::new(lit_with('a', span_range(pat, 11..12))),
                }),
            ])));

        // Test that whitespace after an escape is OK.
        let pat = r"(?x)\ ";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(span_range(pat, 0..pat.len()), vec![
                flag_set(pat, 0..4, ast::Flag::IgnoreWhitespace, false),
                Ast::Literal(ast::Literal {
                    span: span_range(pat, 4..6),
                    kind: ast::LiteralKind::Special(
                        ast::SpecialLiteralKind::Space),
                    c: ' ',
                }),
            ])));
        // ... but only when `x` mode is enabled.
        let pat = r"\ ";
        assert_eq!(
            parser(pat).parse(),
            Err(ast::Error {
                span: span_range(pat, 0..2),
                kind: ast::ErrorKind::EscapeUnrecognized { c: ' ' },
            }));
    }

    #[test]
    fn parse_newlines() {
        let pat = ".\n.";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(span_range(pat, 0..3), vec![
                Ast::Dot(span_range(pat, 0..1)),
                lit_with('\n', span_range(pat, 1..2)),
                Ast::Dot(span_range(pat, 2..3)),
            ])));

        let pat = "foobar\nbaz\nquux\n";
        assert_eq!(
            parser(pat).parse(),
            Ok(concat_with(span_range(pat, 0..pat.len()), vec![
                lit_with('f', nspan(npos(0, 1, 1), npos(1, 1, 2))),
                lit_with('o', nspan(npos(1, 1, 2), npos(2, 1, 3))),
                lit_with('o', nspan(npos(2, 1, 3), npos(3, 1, 4))),
                lit_with('b', nspan(npos(3, 1, 4), npos(4, 1, 5))),
                lit_with('a', nspan(npos(4, 1, 5), npos(5, 1, 6))),
                lit_with('r', nspan(npos(5, 1, 6), npos(6, 1, 7))),
                lit_with('\n', nspan(npos(6, 1, 7), npos(7, 2, 1))),
                lit_with('b', nspan(npos(7, 2, 1), npos(8, 2, 2))),
                lit_with('a', nspan(npos(8, 2, 2), npos(9, 2, 3))),
                lit_with('z', nspan(npos(9, 2, 3), npos(10, 2, 4))),
                lit_with('\n', nspan(npos(10, 2, 4), npos(11, 3, 1))),
                lit_with('q', nspan(npos(11, 3, 1), npos(12, 3, 2))),
                lit_with('u', nspan(npos(12, 3, 2), npos(13, 3, 3))),
                lit_with('u', nspan(npos(13, 3, 3), npos(14, 3, 4))),
                lit_with('x', nspan(npos(14, 3, 4), npos(15, 3, 5))),
                lit_with('\n', nspan(npos(15, 3, 5), npos(16, 4, 1))),
            ])));
    }

    #[test]
    fn parse_uncounted_repetition() {
        assert_eq!(
            parser(r"*").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..1),
                op: ast::RepetitionOp {
                    span: span(0..1),
                    kind: ast::RepetitionKind::ZeroOrMore,
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"+").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..1),
                op: ast::RepetitionOp {
                    span: span(0..1),
                    kind: ast::RepetitionKind::OneOrMore,
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));

        assert_eq!(
            parser(r"?").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..1),
                op: ast::RepetitionOp {
                    span: span(0..1),
                    kind: ast::RepetitionKind::ZeroOrOne,
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"??").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..2),
                op: ast::RepetitionOp {
                    span: span(0..2),
                    kind: ast::RepetitionKind::ZeroOrOne,
                },
                greedy: false,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"a?").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..2),
                op: ast::RepetitionOp {
                    span: span(1..2),
                    kind: ast::RepetitionKind::ZeroOrOne,
                },
                greedy: true,
                ast: Box::new(lit('a', 0)),
            })));
        assert_eq!(
            parser(r"a?b").parse(),
            Ok(concat(0..3, vec![
                Ast::Repetition(ast::Repetition {
                    span: span(0..2),
                    op: ast::RepetitionOp {
                        span: span(1..2),
                        kind: ast::RepetitionKind::ZeroOrOne,
                    },
                    greedy: true,
                    ast: Box::new(lit('a', 0)),
                }),
                lit('b', 2),
            ])));
        assert_eq!(
            parser(r"a??b").parse(),
            Ok(concat(0..4, vec![
                Ast::Repetition(ast::Repetition {
                    span: span(0..3),
                    op: ast::RepetitionOp {
                        span: span(1..3),
                        kind: ast::RepetitionKind::ZeroOrOne,
                    },
                    greedy: false,
                    ast: Box::new(lit('a', 0)),
                }),
                lit('b', 3),
            ])));
        assert_eq!(
            parser(r"ab?").parse(),
            Ok(concat(0..3, vec![
                lit('a', 0),
                Ast::Repetition(ast::Repetition {
                    span: span(1..3),
                    op: ast::RepetitionOp {
                        span: span(2..3),
                        kind: ast::RepetitionKind::ZeroOrOne,
                    },
                    greedy: true,
                    ast: Box::new(lit('b', 1)),
                }),
            ])));
        assert_eq!(
            parser(r"(ab)?").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..5),
                op: ast::RepetitionOp {
                    span: span(4..5),
                    kind: ast::RepetitionKind::ZeroOrOne,
                },
                greedy: true,
                ast: Box::new(group(0..4, concat(1..3, vec![
                    lit('a', 1),
                    lit('b', 2),
                ]))),
            })));
        assert_eq!(
            parser(r"|?").parse(),
            Ok(alt(0..2, vec![
                Ast::Empty(span(0..0)),
                Ast::Repetition(ast::Repetition {
                    span: span(1..2),
                    op: ast::RepetitionOp {
                        span: span(1..2),
                        kind: ast::RepetitionKind::ZeroOrOne,
                    },
                    greedy: true,
                    ast: Box::new(Ast::Empty(span(1..1))),
                }),
            ])));
    }

    #[test]
    fn parse_counted_repetition() {
        assert_eq!(
            parser(r"{5}").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..3),
                op: ast::RepetitionOp {
                    span: span(0..3),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::Exactly(5)),
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"{5,}").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..4),
                op: ast::RepetitionOp {
                    span: span(0..4),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::AtLeast(5)),
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"{5,9}").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..5),
                op: ast::RepetitionOp {
                    span: span(0..5),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::Bounded(5, 9)),
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"{5}?").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..4),
                op: ast::RepetitionOp {
                    span: span(0..4),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::Exactly(5)),
                },
                greedy: false,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"a{5}").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..4),
                op: ast::RepetitionOp {
                    span: span(1..4),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::Exactly(5)),
                },
                greedy: true,
                ast: Box::new(lit('a', 0)),
            })));
        assert_eq!(
            parser(r"ab{5}").parse(),
            Ok(concat(0..5, vec![
                lit('a', 0),
                Ast::Repetition(ast::Repetition {
                    span: span(1..5),
                    op: ast::RepetitionOp {
                        span: span(2..5),
                        kind: ast::RepetitionKind::Range(
                            ast::RepetitionRange::Exactly(5)),
                    },
                    greedy: true,
                    ast: Box::new(lit('b', 1)),
                }),
            ])));
        assert_eq!(
            parser(r"ab{5}c").parse(),
            Ok(concat(0..6, vec![
                lit('a', 0),
                Ast::Repetition(ast::Repetition {
                    span: span(1..5),
                    op: ast::RepetitionOp {
                        span: span(2..5),
                        kind: ast::RepetitionKind::Range(
                            ast::RepetitionRange::Exactly(5)),
                    },
                    greedy: true,
                    ast: Box::new(lit('b', 1)),
                }),
                lit('c', 5),
            ])));

        assert_eq!(
            parser(r"{ 5 }").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..5),
                op: ast::RepetitionOp {
                    span: span(0..5),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::Exactly(5)),
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser(r"{ 5 , 9 }").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..9),
                op: ast::RepetitionOp {
                    span: span(0..9),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::Bounded(5, 9)),
                },
                greedy: true,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));
        assert_eq!(
            parser_ignore_space(r"{5,9} ?").parse(),
            Ok(Ast::Repetition(ast::Repetition {
                span: span(0..7),
                op: ast::RepetitionOp {
                    span: span(0..7),
                    kind: ast::RepetitionKind::Range(
                        ast::RepetitionRange::Bounded(5, 9)),
                },
                greedy: false,
                ast: Box::new(Ast::Empty(span(0..0))),
            })));

        assert_eq!(
            parser(r"{").parse(),
            Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::CountedRepetitionUnclosed,
            }));
        assert_eq!(
            parser(r"{}").parse(),
            Err(ast::Error {
                span: span(1..1),
                kind: ast::ErrorKind::DecimalEmpty,
            }));
        assert_eq!(
            parser(r"{a").parse(),
            Err(ast::Error {
                span: span(1..1),
                kind: ast::ErrorKind::DecimalEmpty,
            }));
        assert_eq!(
            parser(r"{9999999999}").parse(),
            Err(ast::Error {
                span: span(1..11),
                kind: ast::ErrorKind::DecimalInvalid,
            }));
        assert_eq!(
            parser(r"{9").parse(),
            Err(ast::Error {
                span: span(0..2),
                kind: ast::ErrorKind::CountedRepetitionUnclosed,
            }));
        assert_eq!(
            parser(r"{9,a").parse(),
            Err(ast::Error {
                span: span(3..3),
                kind: ast::ErrorKind::DecimalEmpty,
            }));
        assert_eq!(
            parser(r"{9,9999999999}").parse(),
            Err(ast::Error {
                span: span(3..13),
                kind: ast::ErrorKind::DecimalInvalid,
            }));
        assert_eq!(
            parser(r"{9,").parse(),
            Err(ast::Error {
                span: span(0..3),
                kind: ast::ErrorKind::CountedRepetitionUnclosed,
            }));
        assert_eq!(
            parser(r"{9,11").parse(),
            Err(ast::Error {
                span: span(0..5),
                kind: ast::ErrorKind::CountedRepetitionUnclosed,
            }));
    }

    #[test]
    fn parse_alternate() {
        assert_eq!(
            parser(r"a|b").parse(),
            Ok(Ast::Alternation(ast::Alternation {
                span: span(0..3),
                asts: vec![lit('a', 0), lit('b', 2)],
            })));
        assert_eq!(
            parser(r"(a|b)").parse(),
            Ok(group(0..5, Ast::Alternation(ast::Alternation {
                span: span(1..4),
                asts: vec![lit('a', 1), lit('b', 3)],
            }))));

        assert_eq!(
            parser(r"a|b|c").parse(),
            Ok(Ast::Alternation(ast::Alternation {
                span: span(0..5),
                asts: vec![lit('a', 0), lit('b', 2), lit('c', 4)],
            })));
        assert_eq!(
            parser(r"ax|by|cz").parse(),
            Ok(Ast::Alternation(ast::Alternation {
                span: span(0..8),
                asts: vec![
                    concat(0..2, vec![lit('a', 0), lit('x', 1)]),
                    concat(3..5, vec![lit('b', 3), lit('y', 4)]),
                    concat(6..8, vec![lit('c', 6), lit('z', 7)]),
                ],
            })));
        assert_eq!(
            parser(r"(ax|by|cz)").parse(),
            Ok(group(0..10, Ast::Alternation(ast::Alternation {
                span: span(1..9),
                asts: vec![
                    concat(1..3, vec![lit('a', 1), lit('x', 2)]),
                    concat(4..6, vec![lit('b', 4), lit('y', 5)]),
                    concat(7..9, vec![lit('c', 7), lit('z', 8)]),
                ],
            }))));
        assert_eq!(
            parser(r"(ax|(by|(cz)))").parse(),
            Ok(group(0..14, alt(1..13, vec![
                concat(1..3, vec![lit('a', 1), lit('x', 2)]),
                group(4..13, alt(5..12, vec![
                    concat(5..7, vec![lit('b', 5), lit('y', 6)]),
                    group(8..12, concat(9..11, vec![
                        lit('c', 9),
                        lit('z', 10),
                    ])),
                ])),
            ]))));

        assert_eq!(
            parser(r"|").parse(), Ok(alt(0..1, vec![
                Ast::Empty(span(0..0)), Ast::Empty(span(1..1)),
            ])));
        assert_eq!(
            parser(r"||").parse(), Ok(alt(0..2, vec![
                Ast::Empty(span(0..0)),
                Ast::Empty(span(1..1)),
                Ast::Empty(span(2..2)),
            ])));
        assert_eq!(
            parser(r"a|").parse(), Ok(alt(0..2, vec![
                lit('a', 0), Ast::Empty(span(2..2)),
            ])));
        assert_eq!(
            parser(r"|a").parse(), Ok(alt(0..2, vec![
                Ast::Empty(span(0..0)), lit('a', 1),
            ])));

        assert_eq!(
            parser(r"(|)").parse(), Ok(group(0..3, alt(1..2, vec![
                Ast::Empty(span(1..1)), Ast::Empty(span(2..2)),
            ]))));
        assert_eq!(
            parser(r"(a|)").parse(), Ok(group(0..4, alt(1..3, vec![
                lit('a', 1), Ast::Empty(span(3..3)),
            ]))));
        assert_eq!(
            parser(r"(|a)").parse(), Ok(group(0..4, alt(1..3, vec![
                Ast::Empty(span(1..1)), lit('a', 2),
            ]))));

        assert_eq!(
            parser(r"a|b)").parse(), Err(ast::Error {
                span: span(3..4),
                kind: ast::ErrorKind::GroupUnopened,
            }));
        assert_eq!(
            parser(r"(a|b").parse(), Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::GroupUnclosed,
            }));
    }

    #[test]
    fn parse_unsupported_lookaround() {
        assert_eq!(parser(r"(?=a)").parse(), Err(ast::Error {
            span: span(0..3),
            kind: ast::ErrorKind::UnsupportedLookAround,
        }));
        assert_eq!(parser(r"(?!a)").parse(), Err(ast::Error {
            span: span(0..3),
            kind: ast::ErrorKind::UnsupportedLookAround,
        }));
        assert_eq!(parser(r"(?<=a)").parse(), Err(ast::Error {
            span: span(0..4),
            kind: ast::ErrorKind::UnsupportedLookAround,
        }));
        assert_eq!(parser(r"(?<!a)").parse(), Err(ast::Error {
            span: span(0..4),
            kind: ast::ErrorKind::UnsupportedLookAround,
        }));
    }

    #[test]
    fn parse_group() {
        assert_eq!(parser("(?i)").parse(), Ok(Ast::Flags(ast::SetFlags {
            span: span(0..4),
            flags: ast::Flags {
                span: span(2..3),
                items: vec![ast::FlagsItem {
                    span: span(2..3),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                }],
            },
        })));
        assert_eq!(parser("(?iU)").parse(), Ok(Ast::Flags(ast::SetFlags {
            span: span(0..5),
            flags: ast::Flags {
                span: span(2..4),
                items: vec![
                    ast::FlagsItem {
                        span: span(2..3),
                        kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                    },
                    ast::FlagsItem {
                        span: span(3..4),
                        kind: ast::FlagsItemKind::Flag(ast::Flag::SwapGreed),
                    },
                ],
            },
        })));
        assert_eq!(parser("(?i-U)").parse(), Ok(Ast::Flags(ast::SetFlags {
            span: span(0..6),
            flags: ast::Flags {
                span: span(2..5),
                items: vec![
                    ast::FlagsItem {
                        span: span(2..3),
                        kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                    },
                    ast::FlagsItem {
                        span: span(3..4),
                        kind: ast::FlagsItemKind::Negation,
                    },
                    ast::FlagsItem {
                        span: span(4..5),
                        kind: ast::FlagsItemKind::Flag(ast::Flag::SwapGreed),
                    },
                ],
            },
        })));

        assert_eq!(parser("()").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..2),
            kind: ast::GroupKind::CaptureIndex,
            ast: Box::new(Ast::Empty(span(1..1))),
        })));
        assert_eq!(parser("(a)").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..3),
            kind: ast::GroupKind::CaptureIndex,
            ast: Box::new(lit('a', 1)),
        })));
        assert_eq!(parser("(())").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..4),
            kind: ast::GroupKind::CaptureIndex,
            ast: Box::new(Ast::Group(ast::Group {
                span: span(1..3),
                kind: ast::GroupKind::CaptureIndex,
                ast: Box::new(Ast::Empty(span(2..2))),
            })),
        })));

        assert_eq!(parser("(?:a)").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..5),
            kind: ast::GroupKind::NonCapturing(ast::Flags {
                span: span(2..2),
                items: vec![],
            }),
            ast: Box::new(lit('a', 3)),
        })));

        assert_eq!(parser("(?i:a)").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..6),
            kind: ast::GroupKind::NonCapturing(ast::Flags {
                span: span(2..3),
                items: vec![
                    ast::FlagsItem {
                        span: span(2..3),
                        kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                    },
                ],
            }),
            ast: Box::new(lit('a', 4)),
        })));
        assert_eq!(parser("(?i-U:a)").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..8),
            kind: ast::GroupKind::NonCapturing(ast::Flags {
                span: span(2..5),
                items: vec![
                    ast::FlagsItem {
                        span: span(2..3),
                        kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                    },
                    ast::FlagsItem {
                        span: span(3..4),
                        kind: ast::FlagsItemKind::Negation,
                    },
                    ast::FlagsItem {
                        span: span(4..5),
                        kind: ast::FlagsItemKind::Flag(ast::Flag::SwapGreed),
                    },
                ],
            }),
            ast: Box::new(lit('a', 6)),
        })));

        assert_eq!(parser("(").parse(), Err(ast::Error {
            span: span(0..1),
            kind: ast::ErrorKind::GroupUnclosed,
        }));
        assert_eq!(parser("(a").parse(), Err(ast::Error {
            span: span(0..1),
            kind: ast::ErrorKind::GroupUnclosed,
        }));
        assert_eq!(parser("(()").parse(), Err(ast::Error {
            span: span(0..1),
            kind: ast::ErrorKind::GroupUnclosed,
        }));
        assert_eq!(parser(")").parse(), Err(ast::Error {
            span: span(0..1),
            kind: ast::ErrorKind::GroupUnopened,
        }));
        assert_eq!(parser("a)").parse(), Err(ast::Error {
            span: span(1..2),
            kind: ast::ErrorKind::GroupUnopened,
        }));
    }

    #[test]
    fn parse_capture_name() {
        assert_eq!(parser("(?P<a>z)").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..8),
            kind: ast::GroupKind::CaptureName(ast::CaptureName {
                span: span(4..5),
                name: s("a"),
            }),
            ast: Box::new(lit('z', 6)),
        })));
        assert_eq!(parser("(?P<abc>z)").parse(), Ok(Ast::Group(ast::Group {
            span: span(0..10),
            kind: ast::GroupKind::CaptureName(ast::CaptureName {
                span: span(4..7),
                name: s("abc"),
            }),
            ast: Box::new(lit('z', 8)),
        })));

        assert_eq!(parser("(?P<").parse(), Err(ast::Error {
            span: span(4..4),
            kind: ast::ErrorKind::GroupNameUnexpectedEof,
        }));
        assert_eq!(parser("(?P<>z)").parse(), Err(ast::Error {
            span: span(4..4),
            kind: ast::ErrorKind::GroupNameEmpty,
        }));
        assert_eq!(parser("(?P<a").parse(), Err(ast::Error {
            span: span(5..5),
            kind: ast::ErrorKind::GroupNameUnexpectedEof,
        }));
        assert_eq!(parser("(?P<ab").parse(), Err(ast::Error {
            span: span(6..6),
            kind: ast::ErrorKind::GroupNameUnexpectedEof,
        }));
        assert_eq!(parser("(?P<0a").parse(), Err(ast::Error {
            span: span(4..5),
            kind: ast::ErrorKind::GroupNameInvalid { c: '0' },
        }));
        assert_eq!(parser("(?P<~").parse(), Err(ast::Error {
            span: span(4..5),
            kind: ast::ErrorKind::GroupNameInvalid { c: '~' },
        }));
        assert_eq!(parser("(?P<abc~").parse(), Err(ast::Error {
            span: span(7..8),
            kind: ast::ErrorKind::GroupNameInvalid { c: '~' },
        }));
    }

    #[test]
    fn parse_flags() {
        assert_eq!(parser("i:").parse_flags(), Ok(ast::Flags {
            span: span(0..1),
            items: vec![ast::FlagsItem {
                span: span(0..1),
                kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
            }],
        }));
        assert_eq!(parser("i)").parse_flags(), Ok(ast::Flags {
            span: span(0..1),
            items: vec![ast::FlagsItem {
                span: span(0..1),
                kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
            }],
        }));

        assert_eq!(parser("isU:").parse_flags(), Ok(ast::Flags {
            span: span(0..3),
            items: vec![
                ast::FlagsItem {
                    span: span(0..1),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                },
                ast::FlagsItem {
                    span: span(1..2),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::DotMatchesNewLine),
                },
                ast::FlagsItem {
                    span: span(2..3),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::SwapGreed),
                },
            ],
        }));

        assert_eq!(parser("-isU:").parse_flags(), Ok(ast::Flags {
            span: span(0..4),
            items: vec![
                ast::FlagsItem {
                    span: span(0..1),
                    kind: ast::FlagsItemKind::Negation,
                },
                ast::FlagsItem {
                    span: span(1..2),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                },
                ast::FlagsItem {
                    span: span(2..3),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::DotMatchesNewLine),
                },
                ast::FlagsItem {
                    span: span(3..4),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::SwapGreed),
                },
            ],
        }));
        assert_eq!(parser("i-sU:").parse_flags(), Ok(ast::Flags {
            span: span(0..4),
            items: vec![
                ast::FlagsItem {
                    span: span(0..1),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive),
                },
                ast::FlagsItem {
                    span: span(1..2),
                    kind: ast::FlagsItemKind::Negation,
                },
                ast::FlagsItem {
                    span: span(2..3),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::DotMatchesNewLine),
                },
                ast::FlagsItem {
                    span: span(3..4),
                    kind: ast::FlagsItemKind::Flag(ast::Flag::SwapGreed),
                },
            ],
        }));

        assert_eq!(parser("isU").parse_flags(), Err(ast::Error {
            span: span(3..3),
            kind: ast::ErrorKind::FlagUnexpectedEof,
        }));
        assert_eq!(parser("isUa:").parse_flags(), Err(ast::Error {
            span: span(3..4),
            kind: ast::ErrorKind::FlagUnrecognized { flag: 'a' },
        }));
        assert_eq!(parser("isUi:").parse_flags(), Err(ast::Error {
            span: span(3..4),
            kind: ast::ErrorKind::FlagDuplicate {
                flag: 'i',
                original: span(0..1),
            },
        }));
        assert_eq!(parser("i-sU-i:").parse_flags(), Err(ast::Error {
            span: span(4..5),
            kind: ast::ErrorKind::FlagRepeatedNegation {
                original: span(1..2),
            },
        }));
    }

    #[test]
    fn parse_flag() {
        assert_eq!(parser("i").parse_flag(), Ok(ast::Flag::CaseInsensitive));
        assert_eq!(parser("m").parse_flag(), Ok(ast::Flag::MultiLine));
        assert_eq!(parser("s").parse_flag(), Ok(ast::Flag::DotMatchesNewLine));
        assert_eq!(parser("U").parse_flag(), Ok(ast::Flag::SwapGreed));
        assert_eq!(parser("u").parse_flag(), Ok(ast::Flag::Unicode));
        assert_eq!(parser("x").parse_flag(), Ok(ast::Flag::IgnoreWhitespace));

        assert_eq!(parser("a").parse_flag(), Err(ast::Error {
            span: span(0..1),
            kind: ast::ErrorKind::FlagUnrecognized { flag: 'a' },
        }));
        assert_eq!(parser("☃").parse_flag(), Err(ast::Error {
            span: span_range("☃", 0..3),
            kind: ast::ErrorKind::FlagUnrecognized { flag: '☃' },
        }));
    }

    #[test]
    fn parse_primitive_non_escape() {
        assert_eq!(
            parser(r".").parse_primitive(),
            Ok(Primitive::Dot(span(0..1))));
        assert_eq!(
            parser(r"^").parse_primitive(),
            Ok(Primitive::Assertion(ast::Assertion {
                span: span(0..1),
                kind: ast::AssertionKind::StartLine,
            })));
        assert_eq!(
            parser(r"$").parse_primitive(),
            Ok(Primitive::Assertion(ast::Assertion {
                span: span(0..1),
                kind: ast::AssertionKind::EndLine,
            })));

        assert_eq!(
            parser(r"a").parse_primitive(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..1),
                kind: ast::LiteralKind::Verbatim,
                c: 'a',
            })));
        assert_eq!(
            parser(r"|").parse_primitive(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..1),
                kind: ast::LiteralKind::Verbatim,
                c: '|',
            })));
        assert_eq!(
            parser(r"☃").parse_primitive(),
            Ok(Primitive::Literal(ast::Literal {
                span: span_range("☃", 0..3),
                kind: ast::LiteralKind::Verbatim,
                c: '☃',
            })));
    }

    #[test]
    fn parse_escape() {
        assert_eq!(
            parser(r"\|").parse_primitive(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..2),
                kind: ast::LiteralKind::Punctuation,
                c: '|',
            })));
        let specials = &[
            (r"\a", '\x07', ast::SpecialLiteralKind::Bell),
            (r"\f", '\x0C', ast::SpecialLiteralKind::FormFeed),
            (r"\t", '\t', ast::SpecialLiteralKind::Tab),
            (r"\n", '\n', ast::SpecialLiteralKind::LineFeed),
            (r"\r", '\r', ast::SpecialLiteralKind::CarriageReturn),
            (r"\v", '\x0B', ast::SpecialLiteralKind::VerticalTab),
        ];
        for &(pat, c, ref kind) in specials {
            assert_eq!(
                parser(pat).parse_primitive(),
                Ok(Primitive::Literal(ast::Literal {
                    span: span(0..2),
                    kind: ast::LiteralKind::Special(kind.clone()),
                    c: c,
                })));
        }
        assert_eq!(
            parser(r"\A").parse_primitive(),
            Ok(Primitive::Assertion(ast::Assertion {
                span: span(0..2),
                kind: ast::AssertionKind::StartText,
            })));
        assert_eq!(
            parser(r"\z").parse_primitive(),
            Ok(Primitive::Assertion(ast::Assertion {
                span: span(0..2),
                kind: ast::AssertionKind::EndText,
            })));
        assert_eq!(
            parser(r"\b").parse_primitive(),
            Ok(Primitive::Assertion(ast::Assertion {
                span: span(0..2),
                kind: ast::AssertionKind::WordBoundary,
            })));
        assert_eq!(
            parser(r"\B").parse_primitive(),
            Ok(Primitive::Assertion(ast::Assertion {
                span: span(0..2),
                kind: ast::AssertionKind::NotWordBoundary,
            })));

        assert_eq!(parser(r"\").parse_escape(), Err(ast::Error {
            span: span(0..1),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\y").parse_escape(), Err(ast::Error {
            span: span(0..2),
            kind: ast::ErrorKind::EscapeUnrecognized { c: 'y' },
        }));
    }

    #[test]
    fn parse_unsupported_backreference() {
        assert_eq!(parser(r"\0").parse_escape(), Err(ast::Error {
            span: span(0..2),
            kind: ast::ErrorKind::UnsupportedBackreference,
        }));
        assert_eq!(parser(r"\9").parse_escape(), Err(ast::Error {
            span: span(0..2),
            kind: ast::ErrorKind::UnsupportedBackreference,
        }));
    }

    #[test]
    fn parse_octal() {
        for i in 0..511 {
            let pat = format!(r"\{:o}", i);
            assert_eq!(
                parser_octal(&pat).parse_escape(),
                Ok(Primitive::Literal(ast::Literal {
                    span: span(0..pat.len()),
                    kind: ast::LiteralKind::Octal,
                    c: ::std::char::from_u32(i).unwrap(),
                })));
        }
        assert_eq!(
            parser_octal(r"\778").parse_escape(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..3),
                kind: ast::LiteralKind::Octal,
                c: '?',
            })));
        assert_eq!(
            parser_octal(r"\7777").parse_escape(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..4),
                kind: ast::LiteralKind::Octal,
                c: '\u{01FF}',
            })));
        assert_eq!(
            parser_octal(r"\778").parse(),
            Ok(Ast::Concat(ast::Concat {
                span: span(0..4),
                asts: vec![
                    Ast::Literal(ast::Literal {
                        span: span(0..3),
                        kind: ast::LiteralKind::Octal,
                        c: '?',
                    }),
                    Ast::Literal(ast::Literal {
                        span: span(3..4),
                        kind: ast::LiteralKind::Verbatim,
                        c: '8',
                    }),
                ],
            })));
        assert_eq!(
            parser_octal(r"\7777").parse(),
            Ok(Ast::Concat(ast::Concat {
                span: span(0..5),
                asts: vec![
                    Ast::Literal(ast::Literal {
                        span: span(0..4),
                        kind: ast::LiteralKind::Octal,
                        c: '\u{01FF}',
                    }),
                    Ast::Literal(ast::Literal {
                        span: span(4..5),
                        kind: ast::LiteralKind::Verbatim,
                        c: '7',
                    }),
                ],
            })));

        assert_eq!(parser_octal(r"\8").parse_escape(), Err(ast::Error {
            span: span(0..2),
            kind: ast::ErrorKind::EscapeUnrecognized { c: '8' },
        }));
    }

    #[test]
    fn parse_hex_two() {
        for i in 0..256 {
            let pat = format!(r"\x{:02x}", i);
            assert_eq!(
                parser(&pat).parse_escape(),
                Ok(Primitive::Literal(ast::Literal {
                    span: span(0..pat.len()),
                    kind: ast::LiteralKind::HexFixed(ast::HexLiteralKind::X),
                    c: ::std::char::from_u32(i).unwrap(),
                })));
        }

        assert_eq!(parser(r"\xF").parse_escape(), Err(ast::Error {
            span: span(3..3),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\xG").parse_escape(), Err(ast::Error {
            span: span(2..3),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\xFG").parse_escape(), Err(ast::Error {
            span: span(3..4),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
    }

    #[test]
    fn parse_hex_four() {
        for i in 0..65536 {
            let c = match ::std::char::from_u32(i) {
                None => continue,
                Some(c) => c,
            };
            let pat = format!(r"\u{:04x}", i);
            assert_eq!(
                parser(&pat).parse_escape(),
                Ok(Primitive::Literal(ast::Literal {
                    span: span(0..pat.len()),
                    kind: ast::LiteralKind::HexFixed(
                        ast::HexLiteralKind::UnicodeShort),
                    c: c,
                })));
        }

        assert_eq!(parser(r"\uF").parse_escape(), Err(ast::Error {
            span: span(3..3),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\uG").parse_escape(), Err(ast::Error {
            span: span(2..3),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\uFG").parse_escape(), Err(ast::Error {
            span: span(3..4),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\uFFG").parse_escape(), Err(ast::Error {
            span: span(4..5),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\uFFFG").parse_escape(), Err(ast::Error {
            span: span(5..6),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));

        assert_eq!(parser(r"\uD800").parse_escape(), Err(ast::Error {
            span: span(2..6),
            kind: ast::ErrorKind::EscapeHexInvalid,
        }));
    }

    #[test]
    fn parse_hex_eight() {
        for i in 0..65536 {
            let c = match ::std::char::from_u32(i) {
                None => continue,
                Some(c) => c,
            };
            let pat = format!(r"\U{:08x}", i);
            assert_eq!(
                parser(&pat).parse_escape(),
                Ok(Primitive::Literal(ast::Literal {
                    span: span(0..pat.len()),
                    kind: ast::LiteralKind::HexFixed(
                        ast::HexLiteralKind::UnicodeLong),
                    c: c,
                })));
        }

        assert_eq!(parser(r"\UF").parse_escape(), Err(ast::Error {
            span: span(3..3),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\UG").parse_escape(), Err(ast::Error {
            span: span(2..3),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\UFG").parse_escape(), Err(ast::Error {
            span: span(3..4),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\UFFG").parse_escape(), Err(ast::Error {
            span: span(4..5),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\UFFFG").parse_escape(), Err(ast::Error {
            span: span(5..6),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\UFFFFG").parse_escape(), Err(ast::Error {
            span: span(6..7),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\UFFFFFG").parse_escape(), Err(ast::Error {
            span: span(7..8),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\UFFFFFFG").parse_escape(), Err(ast::Error {
            span: span(8..9),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\UFFFFFFFG").parse_escape(), Err(ast::Error {
            span: span(9..10),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
    }

    #[test]
    fn parse_hex_brace() {
        assert_eq!(
            parser(r"\u{26c4}").parse_escape(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..8),
                kind: ast::LiteralKind::HexBrace(
                    ast::HexLiteralKind::UnicodeShort),
                c: '⛄',
            })));
        assert_eq!(
            parser(r"\U{26c4}").parse_escape(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..8),
                kind: ast::LiteralKind::HexBrace(
                    ast::HexLiteralKind::UnicodeLong),
                c: '⛄',
            })));
        assert_eq!(
            parser(r"\x{26c4}").parse_escape(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..8),
                kind: ast::LiteralKind::HexBrace(ast::HexLiteralKind::X),
                c: '⛄',
            })));
        assert_eq!(
            parser(r"\x{26C4}").parse_escape(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..8),
                kind: ast::LiteralKind::HexBrace(ast::HexLiteralKind::X),
                c: '⛄',
            })));
        assert_eq!(
            parser(r"\x{10fFfF}").parse_escape(),
            Ok(Primitive::Literal(ast::Literal {
                span: span(0..10),
                kind: ast::LiteralKind::HexBrace(ast::HexLiteralKind::X),
                c: '\u{10FFFF}',
            })));

        assert_eq!(parser(r"\x").parse_escape(), Err(ast::Error {
            span: span(2..2),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\x{").parse_escape(), Err(ast::Error {
            span: span(2..3),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\x{FF").parse_escape(), Err(ast::Error {
            span: span(2..5),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\x{}").parse_escape(), Err(ast::Error {
            span: span(2..4),
            kind: ast::ErrorKind::EscapeHexEmpty,
        }));
        assert_eq!(parser(r"\x{FGF}").parse_escape(), Err(ast::Error {
            span: span(4..5),
            kind: ast::ErrorKind::EscapeHexInvalidDigit { c: 'G' },
        }));
        assert_eq!(parser(r"\x{FFFFFF}").parse_escape(), Err(ast::Error {
            span: span(3..9),
            kind: ast::ErrorKind::EscapeHexInvalid,
        }));
        assert_eq!(parser(r"\x{D800}").parse_escape(), Err(ast::Error {
            span: span(3..7),
            kind: ast::ErrorKind::EscapeHexInvalid,
        }));
        assert_eq!(parser(r"\x{FFFFFFFFF}").parse_escape(), Err(ast::Error {
            span: span(3..12),
            kind: ast::ErrorKind::EscapeHexInvalid,
        }));
    }

    #[test]
    fn parse_decimal() {
        assert_eq!(parser("123").parse_decimal(), Ok(123));
        assert_eq!(parser("0").parse_decimal(), Ok(0));
        assert_eq!(parser("01").parse_decimal(), Ok(1));

        assert_eq!(parser("-1").parse_decimal(), Err(ast::Error {
            span: span(0..0),
            kind: ast::ErrorKind::DecimalEmpty,
        }));
        assert_eq!(parser("").parse_decimal(), Err(ast::Error {
            span: span(0..0),
            kind: ast::ErrorKind::DecimalEmpty,
        }));
        assert_eq!(parser("9999999999").parse_decimal(), Err(ast::Error {
            span: span(0..10),
            kind: ast::ErrorKind::DecimalInvalid,
        }));
    }

    #[test]
    fn parse_set_class() {
        fn set(
            span: Span,
            negated: bool,
            op: ast::ClassSetOp,
        ) -> ast::Class {
            ast::Class::Set(ast::ClassSet {
                span: span,
                negated: negated,
                op: op,
            })
        }

        fn union(span: Span, items: Vec<ast::ClassSetItem>) -> ast::ClassSetOp {
            ast::ClassSetOp::Union(ast::ClassSetUnion {
                span: span,
                items: items,
            })
        }

        fn intersection(
            span: Span,
            lhs: ast::ClassSetOp,
            rhs: ast::ClassSetOp,
        ) -> ast::ClassSetOp {
            ast::ClassSetOp::BinaryOp(ast::ClassSetBinaryOp {
                span: span,
                kind: ast::ClassSetBinaryOpKind::Intersection,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }

        fn difference(
            span: Span,
            lhs: ast::ClassSetOp,
            rhs: ast::ClassSetOp,
        ) -> ast::ClassSetOp {
            ast::ClassSetOp::BinaryOp(ast::ClassSetBinaryOp {
                span: span,
                kind: ast::ClassSetBinaryOpKind::Difference,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }

        fn symdifference(
            span: Span,
            lhs: ast::ClassSetOp,
            rhs: ast::ClassSetOp,
        ) -> ast::ClassSetOp {
            ast::ClassSetOp::BinaryOp(ast::ClassSetBinaryOp {
                span: span,
                kind: ast::ClassSetBinaryOpKind::SymmetricDifference,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }

        fn item(cls: ast::Class) -> ast::ClassSetItem {
            ast::ClassSetItem::Class(Box::new(cls))
        }

        fn item_ascii(cls: ast::ClassAscii) -> ast::ClassSetItem {
            ast::ClassSetItem::Ascii(cls)
        }

        fn lit(span: Span, c: char) -> ast::ClassSetItem {
            ast::ClassSetItem::Literal(ast::Literal {
                span: span,
                kind: ast::LiteralKind::Verbatim,
                c: c,
            })
        }

        fn range(span: Span, start: char, end: char) -> ast::ClassSetItem {
            let pos1 = Position {
                offset: span.start.offset + start.len_utf8(),
                column: span.start.column + 1,
                ..span.start
            };
            let pos2 = Position {
                offset: span.end.offset - end.len_utf8(),
                column: span.end.column - 1,
                ..span.end
            };
            ast::ClassSetItem::Range(ast::ClassSetRange {
                span: span,
                start: ast::Literal {
                    span: Span { end: pos1, ..span },
                    kind: ast::LiteralKind::Verbatim,
                    c: start,
                },
                end: ast::Literal {
                    span: Span { start: pos2, ..span },
                    kind: ast::LiteralKind::Verbatim,
                    c: end,
                },
            })
        }

        fn alnum(span: Span, negated: bool) -> ast::ClassAscii {
            ast::ClassAscii {
                span: span,
                kind: ast::ClassAsciiKind::Alnum,
                negated: negated,
            }
        }

        /*
        fn alnum(span: Span, negated: bool) -> ast::Class {
            ast::Class::Set(ast::ClassSet {
                span: span,
                negated: negated,
                op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                    span: span,
                    items: vec![ast::ClassSetItem::Ascii(ast::ClassAscii {
                        span: span,
                        kind: ast::ClassAsciiKind::Alnum,
                        negated: negated,
                    })],
                }),
            })
        }
        */

        fn lower(span: Span, negated: bool) -> ast::ClassAscii {
            ast::ClassAscii {
                span: span,
                kind: ast::ClassAsciiKind::Lower,
                negated: negated,
            }
        }

        assert_eq!(
            parser("[[:alnum:]]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..11),
                negated: false,
                op: union(span(1..10), vec![
                    item_ascii(alnum(span(1..10), false)),
                ]),
            }))));
        assert_eq!(
            parser("[[[:alnum:]]]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..13),
                negated: false,
                op: union(span(1..12), vec![
                    item(ast::Class::Set(ast::ClassSet {
                        span: span(1..12),
                        negated: false,
                        op: union(span(2..11), vec![
                            item_ascii(alnum(span(2..11), false)),
                        ]),
                    })),
                ]),
            }))));
        assert_eq!(
            parser("[[:alnum:]&&[:lower:]]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..22),
                negated: false,
                op: intersection(
                    span(1..21),
                    union(span(1..10), vec![
                        item_ascii(alnum(span(1..10), false)),
                    ]),
                    union(span(12..21), vec![
                        item_ascii(lower(span(12..21), false)),
                    ])
                ),
            }))));
        assert_eq!(
            parser("[[:alnum:]--[:lower:]]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..22),
                negated: false,
                op: difference(
                    span(1..21),
                    union(span(1..10), vec![
                        item_ascii(alnum(span(1..10), false)),
                    ]),
                    union(span(12..21), vec![
                        item_ascii(lower(span(12..21), false)),
                    ])
                ),
            }))));
        assert_eq!(
            parser("[[:alnum:]~~[:lower:]]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..22),
                negated: false,
                op: symdifference(
                    span(1..21),
                    union(span(1..10), vec![
                        item_ascii(alnum(span(1..10), false)),
                    ]),
                    union(span(12..21), vec![
                        item_ascii(lower(span(12..21), false)),
                    ])
                ),
            }))));

        assert_eq!(
            parser("[a]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..3),
                negated: false,
                op: union(span(1..2), vec![lit(span(1..2), 'a')]),
            }))));
        assert_eq!(
            parser(r"[a\]]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..5),
                negated: false,
                op: union(span(1..4), vec![
                    lit(span(1..2), 'a'),
                    ast::ClassSetItem::Literal(ast::Literal {
                        span: span(2..4),
                        kind: ast::LiteralKind::Punctuation,
                        c: ']',
                    }),
                ]),
            }))));
        assert_eq!(
            parser(r"[a\-z]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..6),
                negated: false,
                op: union(span(1..5), vec![
                    lit(span(1..2), 'a'),
                    ast::ClassSetItem::Literal(ast::Literal {
                        span: span(2..4),
                        kind: ast::LiteralKind::Punctuation,
                        c: '-',
                    }),
                    lit(span(4..5), 'z'),
                ]),
            }))));
        assert_eq!(
            parser("[ab]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..4),
                negated: false,
                op: union(span(1..3), vec![
                    lit(span(1..2), 'a'),
                    lit(span(2..3), 'b'),
                ]),
            }))));
        assert_eq!(
            parser("[a-]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..4),
                negated: false,
                op: union(span(1..3), vec![
                    lit(span(1..2), 'a'),
                    lit(span(2..3), '-'),
                ]),
            }))));
        assert_eq!(
            parser("[-a]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..4),
                negated: false,
                op: union(span(1..3), vec![
                    lit(span(1..2), '-'),
                    lit(span(2..3), 'a'),
                ]),
            }))));
        assert_eq!(
            parser(r"[\pL]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..5),
                negated: false,
                op: union(span(1..4), vec![
                    item(ast::Class::Unicode(ast::ClassUnicode {
                        span: span(1..4),
                        negated: false,
                        kind: ast::ClassUnicodeKind::OneLetter('L'),
                    })),
                ]),
            }))));
        assert_eq!(
            parser(r"[\w]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..4),
                negated: false,
                op: union(span(1..3), vec![
                    item(ast::Class::Perl(ast::ClassPerl {
                        span: span(1..3),
                        kind: ast::ClassPerlKind::Word,
                        negated: false,
                    })),
                ]),
            }))));
        assert_eq!(
            parser(r"[a\wz]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..6),
                negated: false,
                op: union(span(1..5), vec![
                    lit(span(1..2), 'a'),
                    item(ast::Class::Perl(ast::ClassPerl {
                        span: span(2..4),
                        kind: ast::ClassPerlKind::Word,
                        negated: false,
                    })),
                    lit(span(4..5), 'z'),
                ]),
            }))));

        assert_eq!(
            parser("[a-z]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..5),
                negated: false,
                op: union(span(1..4), vec![
                    range(span(1..4), 'a', 'z'),
                ]),
            }))));
        assert_eq!(
            parser("[a-cx-z]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..8),
                negated: false,
                op: union(span(1..7), vec![
                    range(span(1..4), 'a', 'c'),
                    range(span(4..7), 'x', 'z'),
                ]),
            }))));
        assert_eq!(
            parser(r"[\w&&a-cx-z]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..12),
                negated: false,
                op: intersection(
                    span(1..11),
                    union(span(1..3), vec![
                        item(ast::Class::Perl(ast::ClassPerl {
                            span: span(1..3),
                            kind: ast::ClassPerlKind::Word,
                            negated: false,
                        })),
                    ]),
                    union(span(5..11), vec![
                        range(span(5..8), 'a', 'c'),
                        range(span(8..11), 'x', 'z'),
                    ]),
                ),
            }))));
        assert_eq!(
            parser(r"[a-cx-z&&\w]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..12),
                negated: false,
                op: intersection(
                    span(1..11),
                    union(span(1..7), vec![
                        range(span(1..4), 'a', 'c'),
                        range(span(4..7), 'x', 'z'),
                    ]),
                    union(span(9..11), vec![
                        item(ast::Class::Perl(ast::ClassPerl {
                            span: span(9..11),
                            kind: ast::ClassPerlKind::Word,
                            negated: false,
                        })),
                    ]),
                ),
            }))));
        assert_eq!(
            parser(r"[a--b--c]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..9),
                negated: false,
                op: difference(
                    span(1..8),
                    difference(
                        span(1..5),
                        union(span(1..2), vec![lit(span(1..2), 'a')]),
                        union(span(4..5), vec![lit(span(4..5), 'b')]),
                    ),
                    union(span(7..8), vec![lit(span(7..8), 'c')]),
                ),
            }))));
        assert_eq!(
            parser(r"[a~~b~~c]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..9),
                negated: false,
                op: symdifference(
                    span(1..8),
                    symdifference(
                        span(1..5),
                        union(span(1..2), vec![lit(span(1..2), 'a')]),
                        union(span(4..5), vec![lit(span(4..5), 'b')]),
                    ),
                    union(span(7..8), vec![lit(span(7..8), 'c')]),
                ),
            }))));
        assert_eq!(
            parser(r"[\^&&^]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..7),
                negated: false,
                op: intersection(
                    span(1..6),
                    union(span(1..3), vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(1..3),
                            kind: ast::LiteralKind::Punctuation,
                            c: '^',
                        }),
                    ]),
                    union(span(5..6), vec![
                        lit(span(5..6), '^'),
                    ]),
                ),
            }))));
        assert_eq!(
            parser(r"[\&&&&]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..7),
                negated: false,
                op: intersection(
                    span(1..6),
                    union(span(1..3), vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(1..3),
                            kind: ast::LiteralKind::Punctuation,
                            c: '&',
                        }),
                    ]),
                    union(span(5..6), vec![
                        lit(span(5..6), '&'),
                    ]),
                ),
            }))));
        assert_eq!(
            parser(r"[&&&&]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..6),
                negated: false,
                op: intersection(
                    span(1..5),
                    intersection(
                        span(1..3),
                        union(span(1..1), vec![]),
                        union(span(3..3), vec![]),
                    ),
                    union(span(5..5), vec![]),
                ),
            }))));

        let pat = "[☃-⛄]";
        assert_eq!(
            parser(pat).parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span_range(pat, 0..9),
                negated: false,
                op: union(span_range(pat, 1..8), vec![
                    ast::ClassSetItem::Range(ast::ClassSetRange {
                        span: span_range(pat, 1..8),
                        start: ast::Literal {
                            span: span_range(pat, 1..4),
                            kind: ast::LiteralKind::Verbatim,
                            c: '☃',
                        },
                        end: ast::Literal {
                            span: span_range(pat, 5..8),
                            kind: ast::LiteralKind::Verbatim,
                            c: '⛄',
                        },
                    }),
                ]),
            }))));

        assert_eq!(
            parser(r"[]]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..3),
                negated: false,
                op: union(span(1..2), vec![lit(span(1..2), ']')]),
            }))));
        assert_eq!(
            parser(r"[]\[]").parse(),
            Ok(Ast::Class(ast::Class::Set(ast::ClassSet {
                span: span(0..5),
                negated: false,
                op: union(span(1..4), vec![
                    lit(span(1..2), ']'),
                    ast::ClassSetItem::Literal(ast::Literal  {
                        span: span(2..4),
                        kind: ast::LiteralKind::Punctuation,
                        c: '[',
                    }),
                ]),
            }))));
        assert_eq!(
            parser(r"[\[]]").parse(),
            Ok(concat(0..5, vec![
                Ast::Class(ast::Class::Set(ast::ClassSet {
                    span: span(0..4),
                    negated: false,
                    op: union(span(1..3), vec![
                        ast::ClassSetItem::Literal(ast::Literal  {
                            span: span(1..3),
                            kind: ast::LiteralKind::Punctuation,
                            c: '[',
                        }),
                    ]),
                })),
                Ast::Literal(ast::Literal {
                    span: span(4..5),
                    kind: ast::LiteralKind::Verbatim,
                    c: ']',
                }),
            ])));

        assert_eq!(
            parser("[").parse(),
            Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser("[[").parse(),
            Err(ast::Error {
                span: span(1..2),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser("[[-]").parse(),
            Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser("[[[:alnum:]").parse(),
            Err(ast::Error {
                span: span(1..2),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser(r"[\b]").parse(),
            Err(ast::Error {
                span: span(1..3),
                kind: ast::ErrorKind::ClassIllegal,
            }));
        assert_eq!(
            parser(r"[\w-a]").parse(),
            Err(ast::Error {
                span: span(1..3),
                kind: ast::ErrorKind::ClassIllegal,
            }));
        assert_eq!(
            parser(r"[a-\w]").parse(),
            Err(ast::Error {
                span: span(3..5),
                kind: ast::ErrorKind::ClassIllegal,
            }));

        assert_eq!(
            parser_ignore_space("[a ").parse(),
            Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser_ignore_space("[a- ").parse(),
            Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
    }

    #[test]
    fn parse_set_class_open() {
        assert_eq!(
            parser("[a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..1),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(1..1),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(1..1),
                    items: vec![],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser_ignore_space("[   a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..4),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(4..4),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(4..4),
                    items: vec![],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser("[^a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..2),
                    negated: true,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(2..2),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(2..2),
                    items: vec![],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser_ignore_space("[ ^ a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..4),
                    negated: true,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(4..4),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(4..4),
                    items: vec![],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser("[-a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..2),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(1..1),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(1..2),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(1..2),
                            kind: ast::LiteralKind::Verbatim,
                            c: '-',
                        }),
                    ],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser_ignore_space("[ - a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..4),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(2..2),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(2..3),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(2..3),
                            kind: ast::LiteralKind::Verbatim,
                            c: '-',
                        }),
                    ],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser("[^-a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..3),
                    negated: true,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(2..2),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(2..3),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(2..3),
                            kind: ast::LiteralKind::Verbatim,
                            c: '-',
                        }),
                    ],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser("[--a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..3),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(1..1),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(1..3),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(1..2),
                            kind: ast::LiteralKind::Verbatim,
                            c: '-',
                        }),
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(2..3),
                            kind: ast::LiteralKind::Verbatim,
                            c: '-',
                        }),
                    ],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser("[]a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..2),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(1..1),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(1..2),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(1..2),
                            kind: ast::LiteralKind::Verbatim,
                            c: ']',
                        }),
                    ],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser_ignore_space("[ ] a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..4),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(2..2),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(2..3),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(2..3),
                            kind: ast::LiteralKind::Verbatim,
                            c: ']',
                        }),
                    ],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser("[^]a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..3),
                    negated: true,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(2..2),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(2..3),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(2..3),
                            kind: ast::LiteralKind::Verbatim,
                            c: ']',
                        }),
                    ],
                };
                Ok((set, union))
            });
        assert_eq!(
            parser("[-]a]").parse_set_class_open(), {
                let set = ast::ClassSet {
                    span: span(0..2),
                    negated: false,
                    op: ast::ClassSetOp::Union(ast::ClassSetUnion {
                        span: span(1..1),
                        items: vec![],
                    }),
                };
                let union = ast::ClassSetUnion {
                    span: span(1..2),
                    items: vec![
                        ast::ClassSetItem::Literal(ast::Literal {
                            span: span(1..2),
                            kind: ast::LiteralKind::Verbatim,
                            c: '-',
                        }),
                    ],
                };
                Ok((set, union))
            });

        assert_eq!(
            parser("[").parse_set_class_open(),
            Err(ast::Error {
                span: span(0..1),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser_ignore_space("[    ").parse_set_class_open(),
            Err(ast::Error {
                span: span(0..5),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser("[^").parse_set_class_open(),
            Err(ast::Error {
                span: span(0..2),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser("[]").parse_set_class_open(),
            Err(ast::Error {
                span: span(0..2),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser("[-").parse_set_class_open(),
            Err(ast::Error {
                span: span(0..2),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
        assert_eq!(
            parser("[--").parse_set_class_open(),
            Err(ast::Error {
                span: span(0..3),
                kind: ast::ErrorKind::ClassUnclosed,
            }));
    }

    #[test]
    fn maybe_parse_ascii_class() {
        assert_eq!(
            parser(r"[:alnum:]").maybe_parse_ascii_class(),
            Some(ast::ClassAscii {
                span: span(0..9),
                kind: ast::ClassAsciiKind::Alnum,
                negated: false,
            }));
        assert_eq!(
            parser(r"[:alnum:]A").maybe_parse_ascii_class(),
            Some(ast::ClassAscii {
                span: span(0..9),
                kind: ast::ClassAsciiKind::Alnum,
                negated: false,
            }));
        assert_eq!(
            parser(r"[:^alnum:]").maybe_parse_ascii_class(),
            Some(ast::ClassAscii {
                span: span(0..10),
                kind: ast::ClassAsciiKind::Alnum,
                negated: true,
            }));

        let p = parser(r"[:");
        assert_eq!(p.maybe_parse_ascii_class(), None);
        assert_eq!(p.offset(), 0);

        let p = parser(r"[:^");
        assert_eq!(p.maybe_parse_ascii_class(), None);
        assert_eq!(p.offset(), 0);

        let p = parser(r"[^:alnum:]");
        assert_eq!(p.maybe_parse_ascii_class(), None);
        assert_eq!(p.offset(), 0);

        let p = parser(r"[:alnnum:]");
        assert_eq!(p.maybe_parse_ascii_class(), None);
        assert_eq!(p.offset(), 0);

        let p = parser(r"[:alnum]");
        assert_eq!(p.maybe_parse_ascii_class(), None);
        assert_eq!(p.offset(), 0);

        let p = parser(r"[:alnum:");
        assert_eq!(p.maybe_parse_ascii_class(), None);
        assert_eq!(p.offset(), 0);
    }

    #[test]
    fn parse_unicode_class() {
        assert_eq!(
            parser(r"\pN").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..3),
                negated: false,
                kind: ast::ClassUnicodeKind::OneLetter('N'),
            })));
        assert_eq!(
            parser(r"\PN").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..3),
                negated: true,
                kind: ast::ClassUnicodeKind::OneLetter('N'),
            })));
        assert_eq!(
            parser(r"\p{N}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..5),
                negated: false,
                kind: ast::ClassUnicodeKind::Named(s("N")),
            })));
        assert_eq!(
            parser(r"\P{N}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..5),
                negated: true,
                kind: ast::ClassUnicodeKind::Named(s("N")),
            })));
        assert_eq!(
            parser(r"\p{Greek}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..9),
                negated: false,
                kind: ast::ClassUnicodeKind::Named(s("Greek")),
            })));

        assert_eq!(
            parser(r"\p{scx:Katakana}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..16),
                negated: false,
                kind: ast::ClassUnicodeKind::NamedValue {
                    op: ast::ClassUnicodeOpKind::Colon,
                    name: s("scx"),
                    value: s("Katakana"),
                },
            })));
        assert_eq!(
            parser(r"\p{scx=Katakana}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..16),
                negated: false,
                kind: ast::ClassUnicodeKind::NamedValue {
                    op: ast::ClassUnicodeOpKind::Equal,
                    name: s("scx"),
                    value: s("Katakana"),
                },
            })));
        assert_eq!(
            parser(r"\p{scx!=Katakana}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..17),
                negated: false,
                kind: ast::ClassUnicodeKind::NamedValue {
                    op: ast::ClassUnicodeOpKind::NotEqual,
                    name: s("scx"),
                    value: s("Katakana"),
                },
            })));

        assert_eq!(
            parser(r"\p{:}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..5),
                negated: false,
                kind: ast::ClassUnicodeKind::NamedValue {
                    op: ast::ClassUnicodeOpKind::Colon,
                    name: s(""),
                    value: s(""),
                },
            })));
        assert_eq!(
            parser(r"\p{=}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..5),
                negated: false,
                kind: ast::ClassUnicodeKind::NamedValue {
                    op: ast::ClassUnicodeOpKind::Equal,
                    name: s(""),
                    value: s(""),
                },
            })));
        assert_eq!(
            parser(r"\p{!=}").parse_escape(),
            Ok(Primitive::Unicode(ast::ClassUnicode {
                span: span(0..6),
                negated: false,
                kind: ast::ClassUnicodeKind::NamedValue {
                    op: ast::ClassUnicodeOpKind::NotEqual,
                    name: s(""),
                    value: s(""),
                },
            })));

        assert_eq!(parser(r"\p").parse_escape(), Err(ast::Error {
            span: span(2..2),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\p{").parse_escape(), Err(ast::Error {
            span: span(3..3),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\p{N").parse_escape(), Err(ast::Error {
            span: span(4..4),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));
        assert_eq!(parser(r"\p{Greek").parse_escape(), Err(ast::Error {
            span: span(8..8),
            kind: ast::ErrorKind::EscapeUnexpectedEof,
        }));

        assert_eq!(
            parser(r"\pNz").parse(),
            Ok(Ast::Concat(ast::Concat {
                span: span(0..4),
                asts: vec![
                    Ast::Class(ast::Class::Unicode(ast::ClassUnicode {
                        span: span(0..3),
                        negated: false,
                        kind: ast::ClassUnicodeKind::OneLetter('N'),
                    })),
                    Ast::Literal(ast::Literal {
                        span: span(3..4),
                        kind: ast::LiteralKind::Verbatim,
                        c: 'z',
                    }),
                ],
            })));
        assert_eq!(
            parser(r"\p{Greek}z").parse(),
            Ok(Ast::Concat(ast::Concat {
                span: span(0..10),
                asts: vec![
                    Ast::Class(ast::Class::Unicode(ast::ClassUnicode {
                        span: span(0..9),
                        negated: false,
                        kind: ast::ClassUnicodeKind::Named(s("Greek")),
                    })),
                    Ast::Literal(ast::Literal {
                        span: span(9..10),
                        kind: ast::LiteralKind::Verbatim,
                        c: 'z',
                    }),
                ],
            })));
    }

    #[test]
    fn parse_perl_class() {
        assert_eq!(
            parser(r"\d").parse_escape(),
            Ok(Primitive::Perl(ast::ClassPerl {
                span: span(0..2),
                kind: ast::ClassPerlKind::Digit,
                negated: false,
            })));
        assert_eq!(
            parser(r"\D").parse_escape(),
            Ok(Primitive::Perl(ast::ClassPerl {
                span: span(0..2),
                kind: ast::ClassPerlKind::Digit,
                negated: true,
            })));
        assert_eq!(
            parser(r"\s").parse_escape(),
            Ok(Primitive::Perl(ast::ClassPerl {
                span: span(0..2),
                kind: ast::ClassPerlKind::Space,
                negated: false,
            })));
        assert_eq!(
            parser(r"\S").parse_escape(),
            Ok(Primitive::Perl(ast::ClassPerl {
                span: span(0..2),
                kind: ast::ClassPerlKind::Space,
                negated: true,
            })));
        assert_eq!(
            parser(r"\w").parse_escape(),
            Ok(Primitive::Perl(ast::ClassPerl {
                span: span(0..2),
                kind: ast::ClassPerlKind::Word,
                negated: false,
            })));
        assert_eq!(
            parser(r"\W").parse_escape(),
            Ok(Primitive::Perl(ast::ClassPerl {
                span: span(0..2),
                kind: ast::ClassPerlKind::Word,
                negated: true,
            })));

        assert_eq!(
            parser(r"\d").parse(),
            Ok(Ast::Class(ast::Class::Perl(ast::ClassPerl {
                span: span(0..2),
                kind: ast::ClassPerlKind::Digit,
                negated: false,
            }))));
        assert_eq!(
            parser(r"\dz").parse(),
            Ok(Ast::Concat(ast::Concat {
                span: span(0..3),
                asts: vec![
                    Ast::Class(ast::Class::Perl(ast::ClassPerl {
                        span: span(0..2),
                        kind: ast::ClassPerlKind::Digit,
                        negated: false,
                    })),
                    Ast::Literal(ast::Literal {
                        span: span(2..3),
                        kind: ast::LiteralKind::Verbatim,
                        c: 'z',
                    }),
                ],
            })));
    }
}
