//! The osmChange reader: a tokenizer over a buffer it refills from the input, and the format's structure on top.

use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flate2::bufread::MultiGzDecoder;
use memchr::{memchr, memmem};

use crate::model::{Location, MemberType};
use crate::osc::error::{OscError, Result, XmlError, XmlPosition};
use crate::osc::value::{decimicro, int, is_plain, push_text, timestamp_ms};
use crate::osc::{Action, Change, Meta, Node, Object, Relation, StringsBuf, Tags, Way};

/// The bytes read from the input at once.
const BUFFER: usize = 256 << 10;
/// The largest piece of markup (one element's start tag, or a comment) the buffer grows to hold.
const MAX_MARKUP: usize = 16 << 20;
/// The most node refs or members an object may have: far above the OpenStreetMap API's 2,000 nodes per way and
/// 32,000 members per relation, and a bound on the memory a damaged file can take.
const MAX_ITEMS: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A start tag, `<name ...>`.
    Open,
    /// An empty-element tag, `<name .../>`.
    Empty,
    /// An end tag, `</name>`.
    Close,
    /// The end of the input.
    Eof,
}

/// A tag. Its name, and its attributes in [`Lexer::attrs`], are ranges of the lexer's buffer, valid until the next
/// token is read.
#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    name: Range<usize>,
    /// The tag's offset in the input.
    offset: u64,
}

#[derive(Debug, Clone)]
struct Attr {
    name: Range<usize>,
    value: Range<usize>,
}

/// What [`scan`] found.
enum Scan {
    /// A tag at `start..end` of the buffer.
    Token {
        kind: Kind,
        name: Range<usize>,
        start: usize,
        end: usize,
    },
    /// White space, a comment or a processing instruction, up to this position.
    Skip(usize),
    /// The buffer ends inside the markup that starts at this position.
    More(usize),
}

/// A syntax error at an offset of the buffer, in the element whose name is at a range of it, if that was read.
struct ScanError(usize, Option<Range<usize>>, XmlError);

/// The classes of bytes, looked up in [`CLASS`]: XML white space, and the bytes that end a name.
const SPACE: u8 = 1;
const NAME_END: u8 = 2;
static CLASS: [u8; 256] = {
    let mut class = [0; 256];
    let mut b = 0u8;
    loop {
        class[b as usize] = match b {
            b' ' | b'\t' | b'\n' | b'\r' => SPACE | NAME_END,
            b'/' | b'>' | b'=' | b'<' | b'"' | b'\'' => NAME_END,
            _ => 0,
        };
        if b == u8::MAX {
            break class;
        }
        b += 1;
    }
};

/// The end of the name that starts at `i`, or `None` if the buffer ends first.
fn name_end(buf: &[u8], mut i: usize) -> Option<usize> {
    while i < buf.len() {
        if CLASS[buf[i] as usize] & NAME_END != 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The position after the white space at `i`.
fn skip_space(buf: &[u8], mut i: usize) -> usize {
    while i < buf.len() && CLASS[buf[i] as usize] & SPACE != 0 {
        i += 1;
    }
    i
}

/// Reads the markup at `pos` of `buf`: the white space before it, then a tag (its attributes into `attrs`), a comment
/// or a processing instruction.
fn scan(buf: &[u8], pos: usize, attrs: &mut Vec<Attr>) -> Result<Scan, ScanError> {
    let start = skip_space(buf, pos);
    if buf.get(start).is_some_and(|&b| b != b'<') {
        return Err(ScanError(start, None, XmlError::Text));
    }
    let more = Ok(Scan::More(start));
    let m = &buf[start..];
    let syntax =
        |name: Option<Range<usize>>, what| Err(ScanError(start, name, XmlError::Syntax(what)));
    let skip_to = |from: usize, end: &[u8]| {
        Ok(memmem::find(&buf[from..], end)
            .map_or(Scan::More(start), |i| Scan::Skip(from + i + end.len())))
    };
    match m {
        [] => Ok(Scan::Skip(start)),
        [_] => more,
        [_, b'?', ..] => skip_to(start + 2, b"?>"),
        [_, b'!', ..] if m.starts_with(b"<!--") => skip_to(start + 4, b"-->"),
        [_, b'!', ..] if b"<!--".starts_with(m) => more,
        [_, b'!', ..] => Err(ScanError(start, None, XmlError::Declaration)),
        [_, b'/', ..] => {
            let Some(end) = name_end(buf, start + 2) else {
                return more;
            };
            let name = start + 2..end;
            if name.is_empty() {
                return syntax(None, "expected an element name after </");
            }
            let i = skip_space(buf, end);
            match buf.get(i) {
                None => more,
                Some(b'>') => Ok(Scan::Token {
                    kind: Kind::Close,
                    name,
                    start,
                    end: i + 1,
                }),
                Some(_) => syntax(Some(name), "expected > after the name of an end tag"),
            }
        }
        _ => start_tag(buf, start, attrs),
    }
}

/// Reads the start tag or empty-element tag at `start`, its attributes into `attrs`.
fn start_tag(buf: &[u8], start: usize, attrs: &mut Vec<Attr>) -> Result<Scan, ScanError> {
    let more = Ok(Scan::More(start));
    let syntax =
        |name: Range<usize>, what| Err(ScanError(start, Some(name), XmlError::Syntax(what)));
    let Some(end) = name_end(buf, start + 1) else {
        return more;
    };
    let name = start + 1..end;
    if name.is_empty() {
        return Err(ScanError(
            start,
            None,
            XmlError::Syntax("expected an element name after <"),
        ));
    }
    attrs.clear();
    let mut i = end;
    loop {
        let after_name = i;
        i = skip_space(buf, i);
        let token = |kind, end| {
            Ok(Scan::Token {
                kind,
                name: name.clone(),
                start,
                end,
            })
        };
        match buf.get(i) {
            None => return more,
            Some(b'>') => return token(Kind::Open, i + 1),
            Some(b'/') => {
                return match buf.get(i + 1) {
                    None => more,
                    Some(b'>') => token(Kind::Empty, i + 2),
                    Some(_) => syntax(name, "expected > after /"),
                };
            }
            Some(_) if i == after_name => {
                return syntax(name, "expected white space before an attribute");
            }
            Some(_) => {}
        }
        let Some(attr_end) = name_end(buf, i) else {
            return more;
        };
        if attr_end == i {
            return syntax(name, "expected an attribute name");
        }
        let mut j = skip_space(buf, attr_end);
        match buf.get(j) {
            None => return more,
            Some(b'=') => j = skip_space(buf, j + 1),
            Some(_) => return syntax(name, "expected = after an attribute name"),
        }
        let quote = match buf.get(j) {
            None => return more,
            Some(&q @ (b'"' | b'\'')) => q,
            Some(_) => return syntax(name, "expected a quoted attribute value"),
        };
        let Some(value_end) = memchr(quote, &buf[j + 1..]).map(|len| j + 1 + len) else {
            return more;
        };
        attrs.push(Attr {
            name: i..attr_end,
            value: j + 1..value_end,
        });
        i = value_end + 1;
    }
}

/// Why the lexer stopped.
enum LexError {
    Io(io::Error),
    Xml(ScanError),
}

/// Splits the input into tags, reading it into a buffer that keeps the markup being read whole.
struct Lexer<R> {
    input: R,
    /// Holds the input from `base` on; `buf[..end]` is read.
    buf: Vec<u8>,
    end: usize,
    pos: usize,
    base: u64,
    eof: bool,
    /// The attributes of the last start tag.
    attrs: Vec<Attr>,
}

impl<R: Read> Lexer<R> {
    fn next(&mut self) -> Result<Token, LexError> {
        // A UTF-8 byte order mark before the XML declaration, which some writers add.
        if self.base == 0 && self.pos == 0 {
            while self.end < 3 && !self.eof {
                self.fill()?;
            }
            if self.buf[..self.end].starts_with(b"\xEF\xBB\xBF") {
                self.pos = 3;
            }
        }
        loop {
            match scan(&self.buf[..self.end], self.pos, &mut self.attrs).map_err(LexError::Xml)? {
                Scan::Token {
                    kind,
                    name,
                    start,
                    end,
                } => {
                    self.pos = end;
                    let offset = self.base + start as u64;
                    return Ok(Token { kind, name, offset });
                }
                Scan::Skip(pos) => {
                    self.pos = pos;
                    if pos < self.end {
                        continue;
                    }
                    if self.eof {
                        let offset = self.base + pos as u64;
                        return Ok(Token {
                            kind: Kind::Eof,
                            name: 0..0,
                            offset,
                        });
                    }
                }
                Scan::More(pos) => {
                    self.pos = pos;
                    if self.eof {
                        return Err(LexError::Xml(ScanError(pos, None, XmlError::Truncated)));
                    }
                }
            }
            self.fill()?;
        }
    }

    /// Moves the unread bytes to the front of the buffer, growing it if they fill it, and reads more after them.
    fn fill(&mut self) -> Result<(), LexError> {
        if self.pos > 0 {
            self.buf.copy_within(self.pos..self.end, 0);
            self.base += self.pos as u64;
            self.end -= self.pos;
            self.pos = 0;
        }
        if self.end == self.buf.len() {
            if self.buf.len() >= MAX_MARKUP {
                return Err(LexError::Xml(ScanError(0, None, XmlError::TooLarge)));
            }
            self.buf.resize(self.buf.len() * 2, 0);
        }
        let n = loop {
            match self.input.read(&mut self.buf[self.end..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(LexError::Io(e)),
            }
        };
        self.end += n;
        self.eof = n == 0;
        Ok(())
    }
}

/// Where the reader is in the file's structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Before the `osmChange` element.
    Start,
    /// In the `osmChange` element.
    Root,
    /// In an action section.
    Section(Action),
    /// After the `osmChange` element.
    End,
    /// After the end of the input, or an error.
    Done,
}

/// The reusable buffers objects are read into.
#[derive(Debug, Default)]
struct Scratch {
    tags: StringsBuf,
    roles: StringsBuf,
    refs: Vec<i64>,
    members: Vec<(MemberType, i64)>,
    /// The user name of the last object, shared with the next object of the same user.
    user: Option<Arc<str>>,
    text: String,
    /// The last timestamp parsed, which the next object often has too.
    timestamp: Option<([u8; 20], i64)>,
}

impl Scratch {
    fn timestamp_ms(&mut self, raw: &[u8]) -> Option<i64> {
        if let Some((last, ms)) = &self.timestamp
            && raw == last
        {
            return Some(*ms);
        }
        let ms = timestamp_ms(raw)?;
        self.timestamp = Some((raw.try_into().ok()?, ms));
        Some(ms)
    }

    fn tag<'a>(
        &mut self,
        attrs: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    ) -> Result<(), XmlError> {
        let (mut k, mut v) = (None, None);
        for (name, value) in attrs {
            match name {
                b"k" => set(&mut k, "k", value, Some)?,
                b"v" => set(&mut v, "v", value, Some)?,
                _ => {}
            }
        }
        self.tags.push(k.ok_or(XmlError::MissingAttribute("k"))?)?;
        self.tags.push(v.ok_or(XmlError::MissingAttribute("v"))?)
    }

    fn nd<'a>(
        &mut self,
        attrs: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    ) -> Result<(), XmlError> {
        let mut r = None;
        for (name, value) in attrs {
            if name == b"ref" {
                set(&mut r, "ref", value, int)?;
            }
        }
        if self.refs.len() >= MAX_ITEMS {
            return Err(XmlError::TooLarge);
        }
        self.refs.push(r.ok_or(XmlError::MissingAttribute("ref"))?);
        Ok(())
    }

    fn member<'a>(
        &mut self,
        attrs: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    ) -> Result<(), XmlError> {
        let (mut kind, mut r, mut role) = (None, None, None);
        for (name, value) in attrs {
            match name {
                b"type" => set(&mut kind, "type", value, |v| match v {
                    b"node" => Some(MemberType::Node),
                    b"way" => Some(MemberType::Way),
                    b"relation" => Some(MemberType::Relation),
                    _ => None,
                })?,
                b"ref" => set(&mut r, "ref", value, int)?,
                b"role" => set(&mut role, "role", value, Some)?,
                _ => {}
            }
        }
        let kind = kind.ok_or(XmlError::MissingAttribute("type"))?;
        let r = r.ok_or(XmlError::MissingAttribute("ref"))?;
        if self.members.len() >= MAX_ITEMS {
            return Err(XmlError::TooLarge);
        }
        self.roles.push(role.unwrap_or_default())?;
        self.members.push((kind, r));
        Ok(())
    }
}

/// The attributes of the lexer's last start tag, as (name, value) bytes.
fn attrs<R>(lexer: &Lexer<R>) -> impl Iterator<Item = (&[u8], &[u8])> {
    let buf = &lexer.buf;
    lexer
        .attrs
        .iter()
        .map(move |a| (&buf[a.name.clone()], &buf[a.value.clone()]))
}

/// The attributes of a node, way or relation.
#[derive(Debug, Default)]
struct Head {
    id: Option<i64>,
    meta: Meta,
    /// Checked to be a boolean and not repeated, then ignored: the section decides the action.
    visible: Option<bool>,
    lat: Option<i32>,
    lon: Option<i32>,
}

/// Reads the changes of an osmChange file, in file order.
///
/// The structure is checked strictly: the root element is `osmChange`, it holds only `create`, `modify` and
/// `delete` sections, and those hold only nodes, ways and relations, with only their tags, way nodes and relation
/// members inside. DTDs and entity declarations are rejected; comments and processing instructions are skipped. The
/// section decides the action: a `visible` attribute must be a boolean, but writers disagree on its value in a
/// section (some mark deleted objects visible), so it is not compared with the section. Unknown attributes are
/// ignored.
///
/// After an error, the iterator ends.
pub struct OscReader<R> {
    lexer: Lexer<R>,
    path: Option<PathBuf>,
    level: Level,
    scratch: Scratch,
}

impl OscReader<Box<dyn Read + Send>> {
    /// Opens an osmChange file, decompressing it if it is gzipped, whatever its name.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OscError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| OscError::File {
            path: path.to_owned(),
            source,
        })?;
        // gzip by its magic bytes, whatever the file is called.
        let mut file = BufReader::new(file);
        let gzip = file
            .fill_buf()
            .map_err(|source| OscError::File {
                path: path.to_owned(),
                source,
            })?
            .starts_with(&[0x1f, 0x8b]);
        let input: Box<dyn Read + Send> = if gzip {
            Box::new(MultiGzDecoder::new(file))
        } else {
            Box::new(file)
        };
        let mut reader = Self::new(input);
        reader.path = Some(path.to_owned());
        Ok(reader)
    }
}

impl<R: Read> OscReader<R> {
    /// Reads osmChange XML from `input`, uncompressed. The reader buffers its input itself, so `input` needs no
    /// buffering of its own.
    pub fn new(input: R) -> Self {
        Self {
            lexer: Lexer {
                input,
                buf: vec![0; BUFFER],
                end: 0,
                pos: 0,
                base: 0,
                eof: false,
                attrs: Vec::new(),
            },
            path: None,
            level: Level::Start,
            scratch: Scratch::default(),
        }
    }

    fn token(&mut self) -> Result<Token> {
        self.lexer.next().map_err(|e| match e {
            LexError::Io(source) => match &self.path {
                Some(path) => OscError::File {
                    path: path.clone(),
                    source,
                },
                None => OscError::Io(source),
            },
            LexError::Xml(ScanError(at, name, kind)) => {
                let element =
                    name.map(|n| String::from_utf8_lossy(&self.lexer.buf[n]).into_owned());
                self.error_at(self.lexer.base + at as u64, element, kind)
            }
        })
    }

    fn error_at(&self, offset: u64, element: Option<String>, kind: XmlError) -> OscError {
        let file = self.path.clone();
        let position = XmlPosition {
            file,
            offset,
            element,
        };
        OscError::Xml { position, kind }
    }

    /// An error in the element of `token`.
    fn error(&self, token: &Token, kind: XmlError) -> OscError {
        let name = &self.lexer.buf[token.name.clone()];
        let element = (token.kind != Kind::Eof).then(|| String::from_utf8_lossy(name).into_owned());
        self.error_at(token.offset, element, kind)
    }

    fn name(&self, token: &Token) -> &[u8] {
        &self.lexer.buf[token.name.clone()]
    }

    /// Reads up to the next object, and returns its change; `None` at the end of the file.
    fn advance(&mut self) -> Result<Option<Change>> {
        loop {
            let token = self.token()?;
            let open = matches!(token.kind, Kind::Open | Kind::Empty);
            let empty = token.kind == Kind::Empty;
            let name = self.name(&token);
            let unexpected = |what| Err(self.error(&token, XmlError::Unexpected(what)));
            match (self.level, token.kind) {
                (Level::Start, _) if open && name == b"osmChange" => {
                    self.level = if empty { Level::End } else { Level::Root };
                }
                (Level::Root, _) if open => {
                    let action = match name {
                        b"create" => Action::Create,
                        b"modify" => Action::Modify,
                        b"delete" => Action::Delete,
                        _ => {
                            return unexpected(
                                "in <osmChange>, which holds only <create>, <modify> and <delete>",
                            );
                        }
                    };
                    if !empty {
                        self.level = Level::Section(action);
                    }
                }
                (Level::Section(action), _) if open => {
                    let kind = match name {
                        b"node" => MemberType::Node,
                        b"way" => MemberType::Way,
                        b"relation" => MemberType::Relation,
                        _ => {
                            return unexpected(
                                "in an action section, which holds only <node>, <way> and <relation>",
                            );
                        }
                    };
                    return self.object(action, kind, &token).map(Some);
                }
                (Level::Root, Kind::Close) if name == b"osmChange" => self.level = Level::End,
                (Level::Section(action), Kind::Close) if name == action.name().as_bytes() => {
                    self.level = Level::Root;
                }
                (Level::End | Level::Done, Kind::Eof) => {
                    self.level = Level::Done;
                    return Ok(None);
                }
                (_, Kind::Eof) => return Err(self.error(&token, XmlError::Truncated)),
                (Level::Start, _) => {
                    return unexpected("as the root element, which must be <osmChange>");
                }
                (Level::End | Level::Done, _) => return unexpected("after the end of <osmChange>"),
                (Level::Root, _) => {
                    return Err(self.error(&token, XmlError::Mismatched("osmChange")));
                }
                (Level::Section(action), _) => {
                    return Err(self.error(&token, XmlError::Mismatched(action.name())));
                }
            }
        }
    }

    /// Reads a node, way or relation from its start tag `token` to its end.
    fn object(&mut self, action: Action, kind: MemberType, token: &Token) -> Result<Change> {
        // Reading the children may move the buffer, so errors name the object by its type, not by the token's range.
        let name = match kind {
            MemberType::Node => "node",
            MemberType::Way => "way",
            MemberType::Relation => "relation",
        };
        let error = |reader: &Self, e| reader.error_at(token.offset, Some(name.to_owned()), e);
        let head = self.head().map_err(|e| error(self, e))?;
        let s = &mut self.scratch;
        s.tags.clear();
        s.refs.clear();
        s.members.clear();
        s.roles.clear();
        if token.kind == Kind::Open {
            self.children(kind, name)?;
        }
        self.finish(action, kind, head).map_err(|e| error(self, e))
    }

    /// Parses the attributes of a node, way or relation.
    fn head(&mut self) -> Result<Head, XmlError> {
        let Self { lexer, scratch, .. } = self;
        let mut head = Head::default();
        let m = &mut head.meta;
        for attr in &lexer.attrs {
            let v = &lexer.buf[attr.value.clone()];
            match &lexer.buf[attr.name.clone()] {
                b"id" => set(&mut head.id, "id", v, int)?,
                b"version" => set(&mut m.version, "version", v, int)?,
                b"timestamp" => set(&mut m.timestamp_ms, "timestamp", v, |v| {
                    scratch.timestamp_ms(v)
                })?,
                b"changeset" => set(&mut m.changeset, "changeset", v, int)?,
                b"uid" => set(&mut m.uid, "uid", v, int)?,
                b"lat" => set(&mut head.lat, "lat", v, |v| {
                    decimicro(v).filter(|lat| lat.unsigned_abs() <= 900_000_000)
                })?,
                b"lon" => set(&mut head.lon, "lon", v, |v| {
                    decimicro(v).filter(|lon| lon.unsigned_abs() <= 1_800_000_000)
                })?,
                b"visible" => set(&mut head.visible, "visible", v, |v| match v {
                    b"true" => Some(true),
                    b"false" => Some(false),
                    _ => None,
                })?,
                b"user" if m.user.is_some() => return Err(XmlError::DuplicateAttribute("user")),
                b"user" => m.user = Some(user(&mut scratch.user, &mut scratch.text, v)?),
                _ => {}
            }
        }
        Ok(head)
    }

    /// Reads the children of a node, way or relation into the scratch buffers, up to its end tag.
    fn children(&mut self, kind: MemberType, parent: &'static str) -> Result<()> {
        let allowed = match kind {
            MemberType::Node => "in <node>, which holds only <tag>",
            MemberType::Way => "in <way>, which holds only <tag> and <nd>",
            MemberType::Relation => "in <relation>, which holds only <tag> and <member>",
        };
        loop {
            let token = self.token()?;
            let name = self.name(&token);
            let child = match (token.kind, name, kind) {
                (Kind::Close, _, _) if name == parent.as_bytes() => return Ok(()),
                (Kind::Close, _, _) => Err(XmlError::Mismatched(parent)),
                (Kind::Eof, _, _) => Err(XmlError::Truncated),
                (_, b"tag", _) => self.scratch.tag(attrs(&self.lexer)).map(|()| "tag"),
                (_, b"nd", MemberType::Way) => self.scratch.nd(attrs(&self.lexer)).map(|()| "nd"),
                (_, b"member", MemberType::Relation) => {
                    self.scratch.member(attrs(&self.lexer)).map(|()| "member")
                }
                _ => Err(XmlError::Unexpected(allowed)),
            };
            let child = child.map_err(|e| self.error(&token, e))?;
            if token.kind == Kind::Open {
                // A child written as a start and an end tag, which must follow at once. Reading it may move the
                // buffer, so the child's name is compared as text, not as a range of the buffer.
                let end = self.token()?;
                if end.kind != Kind::Close || self.name(&end) != child.as_bytes() {
                    return Err(
                        self.error(&end, XmlError::Unexpected("where an end tag was expected"))
                    );
                }
            }
        }
    }

    /// Builds the object read into `head` and the scratch buffers.
    fn finish(&self, action: Action, kind: MemberType, head: Head) -> Result<Change, XmlError> {
        let id = head.id.ok_or(XmlError::MissingAttribute("id"))?;
        let s = &self.scratch;
        let (meta, tags) = (head.meta, Tags(s.tags.to_strings()));
        let object = match kind {
            MemberType::Node => {
                let location = match (head.lat, head.lon) {
                    (Some(lat), Some(lon)) => Some(Location { lat, lon }),
                    (None, None) if action == Action::Delete => None,
                    (None, None) => return Err(XmlError::NoLocation),
                    _ => return Err(XmlError::HalfLocation),
                };
                Object::Node(Node {
                    id,
                    meta,
                    tags,
                    location,
                })
            }
            MemberType::Way => Object::Way(Way {
                id,
                meta,
                tags,
                refs: s.refs.clone(),
            }),
            MemberType::Relation => Object::Relation(Relation {
                id,
                meta,
                tags,
                members: s.members.as_slice().into(),
                roles: s.roles.to_strings(),
            }),
        };
        Ok(Change { action, object })
    }
}

impl<R: Read> Iterator for OscReader<R> {
    type Item = Result<Change, OscError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.level == Level::Done {
            return None;
        }
        match self.advance() {
            Ok(change) => change.map(Ok),
            Err(e) => {
                self.level = Level::Done;
                Some(Err(e))
            }
        }
    }
}

/// Parses an attribute into its slot, which must still be empty.
fn set<'a, T>(
    slot: &mut Option<T>,
    name: &'static str,
    value: &'a [u8],
    parse: impl FnOnce(&'a [u8]) -> Option<T>,
) -> Result<(), XmlError> {
    if slot.is_some() {
        return Err(XmlError::DuplicateAttribute(name));
    }
    *slot = Some(parse(value).ok_or_else(|| XmlError::Value {
        attribute: name,
        value: String::from_utf8_lossy(value).into_owned(),
    })?);
    Ok(())
}

/// A user name, shared with the last object's when it is the same. Only a plain name is compared raw: a reference, a
/// line break or a `<` makes its text differ from its bytes, or makes it an error.
fn user(last: &mut Option<Arc<str>>, text: &mut String, raw: &[u8]) -> Result<Arc<str>, XmlError> {
    if let Some(last) = last
        && is_plain(raw)
        && last.as_bytes() == raw
    {
        return Ok(Arc::clone(last));
    }
    text.clear();
    push_text(raw, text)?;
    let name: Arc<str> = text.as_str().into();
    *last = Some(Arc::clone(&name));
    Ok(name)
}
