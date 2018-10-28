use std::fmt;
use std::error;
use std::str;
use std::mem;
use std::borrow::Cow;

use xmlparser::{
    self,
    TextPos,
    Reference,
    Stream,
    StrSpan,
};

use {
    NS_XMLNS_URI,
    NS_XML_URI,
    Attribute,
    Document,
    ExpandedNameOwned,
    Namespaces,
    Node,
    NodeData,
    NodeId,
    NodeKind,
    PI,
};

const ENTITY_DEPTH: u8 = 10;


/// A list of all possible errors.
#[derive(Debug)]
pub enum Error {
    /// The `xmlns:xml` attribute must have an <http://www.w3.org/XML/1998/namespace> URI.
    InvalidXmlPrefixUri(TextPos),

    /// Only the `xmlns:xml` attribute can have the <http://www.w3.org/XML/1998/namespace> URI.
    UnexpectedXmlUri(TextPos),

    /// The <http://www.w3.org/2000/xmlns/> URI must not be declared.
    UnexpectedXmlnsUri(TextPos),

    /// `xmlns` can't be used as an element prefix.
    InvalidElementNamePrefix(TextPos),

    /// A namespace was already defined on this element.
    DuplicatedNamespace(String, TextPos),

    /// Incorrect tree structure.
    #[allow(missing_docs)]
    UnexpectedCloseTag { expected: String, actual: String, pos: TextPos },

    /// Entity value starts with a close tag.
    ///
    /// Example:
    /// ```xml
    /// <!DOCTYPE test [ <!ENTITY p '</p>'> ]>
    /// <root>&p;</root>
    /// ```
    UnexpectedEntityCloseTag(TextPos),

    /// A reference to an entity that was not defined in the DTD.
    UnknownEntityReference(String, TextPos),

    /// A possible entity reference loop.
    EntityReferenceLoop(TextPos),

    /// An element has a duplicated attributes.
    ///
    /// This also includes namespaces resolving.
    /// So an element like this will lead to an error.
    /// ```xml
    /// <e xmlns:n1='http://www.w3.org' xmlns:n2='http://www.w3.org' n1:a='b1' n2:a='b2'/>
    /// ```
    DuplicatedAttribute(String, TextPos),

    /// The XML document must have at least one element.
    NoRootNode,

    /// Errors detected by the `xmlparser` crate.
    ParserError(xmlparser::Error),
}

impl From<xmlparser::Error> for Error {
    fn from(e: xmlparser::Error) -> Self {
        Error::ParserError(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::InvalidXmlPrefixUri(pos) => {
                write!(f, "'xml' namespace prefix mapped to wrong URI at {}", pos)
            }
            Error::UnexpectedXmlUri(pos) => {
                write!(f, "the 'xml' namespace URI is used for not 'xml' prefix at {}", pos)
            }
            Error::UnexpectedXmlnsUri(pos) => {
                write!(f, "the 'xmlns' URI is used at {}, but it must not be declared", pos)
            }
            Error::InvalidElementNamePrefix(pos) => {
                write!(f, "the 'xmlns' prefix is used at {}, but it must not be", pos)
            }
            Error::DuplicatedNamespace(ref name, pos) => {
                write!(f, "namespace '{}' at {} is already defined", name, pos)
            }
            Error::UnexpectedCloseTag { ref expected, ref actual, pos } => {
                write!(f, "expected '{}' tag, not '{}' at {}", expected, actual, pos)
            }
            Error::UnexpectedEntityCloseTag(pos) => {
                write!(f, "unexpected close tag at {}", pos)
            }
            Error::UnknownEntityReference(ref name, pos) => {
                write!(f, "unknown entity reference '{}' at {}", name, pos)
            }
            Error::EntityReferenceLoop(pos) => {
                write!(f, "a possible entity reference loop is detected at {}", pos)
            }
            Error::DuplicatedAttribute(ref name, pos) => {
                write!(f, "attribute '{}' at {} is already defined", name, pos)
            }
            Error::NoRootNode => {
                write!(f, "the document does not have a root node")
            }
            Error::ParserError(ref err) => {
                write!(f, "{}", err)
            }
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        "an XML parsing error"
    }
}


struct AttributeData<'d> {
    prefix: StrSpan<'d>,
    prefix_str: &'d str,
    local: StrSpan<'d>,
    local_str: &'d str,
    value_pos: usize,
    value: Cow<'d, str>,
}

impl<'d> Document<'d> {
    /// Parses the input XML string.
    ///
    /// We do not support `&[u8]` or `Reader` because the input must be an already allocated
    /// UTF-8 string.
    ///
    /// # Examples
    ///
    /// ```
    /// let doc = roxmltree::Document::parse("<e/>").unwrap();
    /// assert_eq!(doc.descendants().count(), 2); // root node + `e` element node
    /// ```
    pub fn parse(text: &str) -> Result<Document, Error> {
        parse(text)
    }

    fn append(&mut self, parent_id: NodeId, kind: NodeKind<'d>, orig_pos: usize) -> NodeId {
        let new_child_id = NodeId(self.nodes.len());
        self.nodes.push(NodeData {
            parent: Some(parent_id),
            prev_sibling: None,
            next_sibling: None,
            children: None,
            kind,
            orig_pos,
        });

        let last_child_id = self.nodes[parent_id.0].children.map(|(_, id)| id);
        self.nodes[new_child_id.0].prev_sibling = last_child_id;

        if let Some(id) = last_child_id {
            self.nodes[id.0].next_sibling = Some(new_child_id);
        }

        self.nodes[parent_id.0].children = Some(
            if let Some((first_child_id, _)) = self.nodes[parent_id.0].children {
                (first_child_id, new_child_id)
            } else {
                (new_child_id, new_child_id)
            }
        );

        new_child_id
    }

    fn get(&self, id: NodeId) -> Node {
        Node { id, d: &self.nodes[id.0], doc: self }
    }
}

struct Entity<'d>  {
    name: &'d str,
    value: StrSpan<'d>,
}

struct ParserData<'d> {
    attrs_start_idx: usize,
    ns_start_idx: usize,
    tmp_attrs: Vec<AttributeData<'d>>,
    entities: Vec<Entity<'d>>,
    buffer: Vec<u8>,
    prev_node_type: Option<xmlparser::Token<'d>>,
}

#[derive(Clone, Copy)]
struct TagNameSpan<'d> {
    prefix: StrSpan<'d>,
    name: StrSpan<'d>,
}

impl<'d> TagNameSpan<'d> {
    fn new_null() -> Self {
        Self { prefix: StrSpan::from(""), name: StrSpan::from("") }
    }

    fn new(prefix: StrSpan<'d>, name: StrSpan<'d>) -> Self {
        Self { prefix, name }
    }
}

fn parse(text: &str) -> Result<Document, Error> {
    let mut pd = ParserData {
        attrs_start_idx: 0,
        ns_start_idx: 1,
        tmp_attrs: Vec::new(),
        entities: Vec::new(),
        buffer: Vec::with_capacity(32),
        prev_node_type: None,
    };

    // Trying to guess rough nodes and attributes amount.
    let nodes_capacity = text.bytes().filter(|c| *c == b'<').count();
    let attributes_capacity = text.bytes().filter(|c| *c == b'=').count();

    // Init document.
    let mut doc = Document {
        text,
        nodes: Vec::with_capacity(nodes_capacity),
        attrs: Vec::with_capacity(attributes_capacity),
        namespaces: Namespaces(Vec::new()),
    };

    // Add a root node.
    doc.nodes.push(NodeData {
        parent: None,
        prev_sibling: None,
        next_sibling: None,
        children: None,
        kind: NodeKind::Root,
        orig_pos: 0,
    });

    doc.namespaces.push_ns(Some("xml"), NS_XML_URI.to_string());

    let parser = xmlparser::Tokenizer::from(text);
    let parent_id = doc.root().id;
    let mut tag_name = TagNameSpan::new_null();
    process_tokens(parser, 0, parent_id, &mut tag_name, &mut pd, &mut doc)?;

    if !doc.root().children().any(|n| n.is_element()) {
        return Err(Error::NoRootNode);
    }

    Ok(doc)
}

fn process_tokens<'d>(
    parser: xmlparser::Tokenizer<'d>,
    depth: u8,
    mut parent_id: NodeId,
    tag_name: &mut TagNameSpan<'d>,
    pd: &mut ParserData<'d>,
    doc: &mut Document<'d>,
) -> Result<(), Error> {
    for token in parser {
        let token = token?;
        match token {
            xmlparser::Token::ProcessingInstruction(target, content) => {
                doc.append(parent_id,
                    NodeKind::PI(PI {
                        target: target.to_str(),
                        value: content.map(|v| v.to_str()),
                    }),
                    target.start() - 2, // jump before '<?'
                );
            }
            xmlparser::Token::Comment(text) => {
                let orig_pos = text.start() - 4; // jump before '<!--'
                doc.append(parent_id, NodeKind::Comment(text.to_str()), orig_pos);
            }
            xmlparser::Token::Text(text) |
            xmlparser::Token::Whitespaces(text) => {
                process_text(text, parent_id, depth, pd, doc)?;
            }
            xmlparser::Token::Cdata(text) => {
                process_cdata(text, parent_id, pd, doc);
            }
            xmlparser::Token::ElementStart(prefix, local) => {
                let prefix_str = prefix.to_str();

                if prefix_str == "xmlns" {
                    let pos = err_pos_from_span(prefix);
                    return Err(Error::InvalidElementNamePrefix(pos));
                }

                *tag_name = TagNameSpan::new(prefix, local);
            }
            xmlparser::Token::Attribute((prefix, local), value) => {
                process_attribute(depth, prefix, local, value, pd, doc)?;
            }
            xmlparser::Token::ElementEnd(end) => {
                process_element(*tag_name, end, &mut parent_id, pd, doc)?;
            }
            xmlparser::Token::EntityDeclaration(name, value) => {
                if let xmlparser::EntityDefinition::EntityValue(value) = value {
                    pd.entities.push(Entity { name: name.to_str(), value });
                }
            }
            _ => {}
        }

        pd.prev_node_type = Some(token);
    }

    Ok(())
}

fn process_attribute<'d>(
    depth: u8,
    prefix: StrSpan<'d>,
    local: StrSpan<'d>,
    value: StrSpan<'d>,
    pd: &mut ParserData<'d>,
    doc: &mut Document<'d>,
) -> Result<(), Error> {
    let prefix_str = prefix.to_str();
    let local_str = local.to_str();
    let orig_pos = value.start();
    let value = normalize_attribute(depth, value, &pd.entities, &mut pd.buffer)?;

    if prefix_str == "xmlns" {
        // The xmlns namespace MUST NOT be declared as the default namespace.
        if value == NS_XMLNS_URI {
            let pos = err_pos_from_qname(prefix, local);
            return Err(Error::UnexpectedXmlnsUri(pos));
        }

        let is_xml_ns_uri = value == NS_XML_URI;

        // The prefix 'xml' is by definition bound to the namespace name
        // http://www.w3.org/XML/1998/namespace.
        // It MUST NOT be bound to any other namespace name.
        if local_str == "xml" {
            if !is_xml_ns_uri {
                let pos = err_pos_from_span(prefix);
                return Err(Error::InvalidXmlPrefixUri(pos));
            }
        } else {
            // The xml namespace MUST NOT be bound to a non-xml prefix.
            if is_xml_ns_uri {
                let pos = err_pos_from_span(prefix);
                return Err(Error::UnexpectedXmlUri(pos));
            }
        }

        // Check for duplicated namespaces.
        if doc.namespaces.exists(pd.ns_start_idx, Some(local_str)) {
            let pos = err_pos_from_qname(prefix, local);
            return Err(Error::DuplicatedNamespace(local_str.to_string(), pos));
        }

        // Xml namespace should not be added to the namespaces.
        if !is_xml_ns_uri {
            doc.namespaces.push_ns(Some(local_str), value.into());
        }
    } else if local_str == "xmlns" {
        // The xml namespace MUST NOT be declared as the default namespace.
        if value == NS_XML_URI {
            let pos = err_pos_from_span(local);
            return Err(Error::UnexpectedXmlUri(pos));
        }

        // The xmlns namespace MUST NOT be declared as the default namespace.
        if value == NS_XMLNS_URI {
            let pos = err_pos_from_span(local);
            return Err(Error::UnexpectedXmlnsUri(pos));
        }

        doc.namespaces.push_ns(None, value.into());
    } else {
        pd.tmp_attrs.push(AttributeData { prefix, prefix_str, local, local_str,
                                          value_pos: orig_pos, value });
    }

    Ok(())
}

fn process_element<'d>(
    tag_name: TagNameSpan<'d>,
    end_token: xmlparser::ElementEnd<'d>,
    parent_id: &mut NodeId,
    pd: &mut ParserData<'d>,
    doc: &mut Document<'d>,
) -> Result<(), Error> {
    if tag_name.name.is_empty() {
        // May occur in XML like this:
        // <!DOCTYPE test [ <!ENTITY p '</p>'> ]>
        // <root>&p;</root>

        if let xmlparser::ElementEnd::Close(prefix, local) = end_token {
            return Err(Error::UnexpectedEntityCloseTag(err_pos_from_tag_name(prefix, local, true)));
        } else {
            unreachable!("should be already checked by the xmlparser");
        }
    }

    // Resolve namespaces.
    let mut tmp_parent_id = parent_id.0;
    while tmp_parent_id != 0 {
        let curr_id = tmp_parent_id;
        tmp_parent_id = match doc.nodes[tmp_parent_id].parent {
            Some(id) => id.0,
            None => 0,
        };

        let ns_range = match doc.nodes[curr_id].kind {
            NodeKind::Element { ref namespaces, .. } => namespaces.clone(),
            _ => continue,
        };

        for i in ns_range {
            if !doc.namespaces.exists(pd.ns_start_idx, doc.namespaces[i].name) {
                let v = doc.namespaces[i].clone();
                doc.namespaces.0.push(v);
            }
        }
    }

    let mut namespaces = 0..0;
    if pd.ns_start_idx != doc.namespaces.len() {
        namespaces = pd.ns_start_idx..doc.namespaces.len();
        pd.ns_start_idx = doc.namespaces.len();
    }


    // Resolve attributes.
    let mut attributes = 0..0;
    if !pd.tmp_attrs.is_empty() {
        for attr in &mut pd.tmp_attrs {
            let ns = if attr.prefix_str == "xml" {
                // The prefix 'xml' is by definition bound to the namespace name
                // http://www.w3.org/XML/1998/namespace.
                Some(doc.namespaces.xml_uri())
            } else if attr.prefix_str.is_empty() {
                // 'The namespace name for an unprefixed attribute name
                // always has no value.'
                None
            } else {
                doc.namespaces.get_by_prefix(namespaces.clone(), attr.prefix_str)
            };

            let attr_name = ExpandedNameOwned { ns, name: attr.local_str };

            // Check for duplicated attributes.
            if doc.attrs[pd.attrs_start_idx..].iter().any(|attr| attr.name == attr_name) {
                let pos = err_pos_from_qname(attr.prefix, attr.local);
                return Err(Error::DuplicatedAttribute(attr.local_str.to_string(), pos));
            }

            let attr_pos = if attr.prefix.is_empty() {
                attr.local.start()
            } else {
                attr.prefix.start()
            };

            doc.attrs.push(Attribute {
                name: attr_name,
                value: mem::replace(&mut attr.value, Cow::Borrowed("")),
                attr_pos,
                value_pos: attr.value_pos,
            });
        }
        attributes = pd.attrs_start_idx..doc.attrs.len();
        pd.attrs_start_idx = doc.attrs.len();
    }
    pd.tmp_attrs.clear();


    let tag_ns_uri = doc.namespaces.get_by_prefix(namespaces.clone(), tag_name.prefix.to_str());
    match end_token {
        xmlparser::ElementEnd::Empty => {
            doc.append(*parent_id,
                NodeKind::Element {
                    tag_name: ExpandedNameOwned {
                        ns: tag_ns_uri,
                        name: tag_name.name.to_str(),
                    },
                    attributes,
                    namespaces,
                },
                orig_pos_from_tag_name(&tag_name)
            );
        }
        xmlparser::ElementEnd::Close(prefix, local) => {
            let prefix_str = prefix.to_str();
            let local_str = local.to_str();

            if let NodeKind::Element { ref tag_name, .. } = doc.nodes[parent_id.0].kind {
                let parent_node = doc.get(*parent_id);
                let parent_prefix = parent_node.resolve_tag_name_prefix().unwrap_or("");

                if prefix_str != parent_prefix || local_str != tag_name.name {
                    return Err(Error::UnexpectedCloseTag {
                        expected: gen_qname_string(parent_prefix, tag_name.name),
                        actual: gen_qname_string(prefix_str, local_str),
                        pos: err_pos_from_tag_name(prefix, local, true),
                    });
                }
            }

            if let Some(id) = doc.nodes[parent_id.0].parent {
                *parent_id = id;
            } else {
                unreachable!("should be already checked by the xmlparser");
            }
        }
        xmlparser::ElementEnd::Open => {
            *parent_id = doc.append(*parent_id,
                NodeKind::Element {
                    tag_name: ExpandedNameOwned {
                        ns: tag_ns_uri,
                        name: tag_name.name.to_str(),
                    },
                    attributes,
                    namespaces,
                },
                orig_pos_from_tag_name(&tag_name)
            );
        }
    }

    Ok(())
}

fn process_text<'d>(
    text: StrSpan<'d>,
    parent_id: NodeId,
    depth: u8,
    pd: &mut ParserData<'d>,
    doc: &mut Document<'d>,
) -> Result<(), Error> {
    let mut depth = depth;
    pd.buffer.clear();

    let mut is_as_is = false;
    let mut s = Stream::from(text);
    while !s.at_end() {
        match parse_next_chunk(&mut s, &pd.entities)? {
            NextChunk::Byte(c) => {
                if is_as_is {
                    pd.buffer.push(c);
                    is_as_is = false;
                } else {
                    push_byte_to_text(c, s.at_end(), &mut pd.buffer);
                }
            }
            NextChunk::Char(c) => {
                for b in CharToBytes::new(c) {
                    if depth > 0 {
                        push_byte_to_text(b, s.at_end(), &mut pd.buffer);
                    } else {
                        // Characters not from entity should be added as is.
                        // Not sure why... At least `lxml` produces the same results.
                        pd.buffer.push(b);
                        is_as_is = true;
                    }
                }
            }
            NextChunk::Text(fragment) => {
                if depth > ENTITY_DEPTH {
                    let pos = s.gen_error_pos();
                    return Err(Error::EntityReferenceLoop(pos));
                }

                if !pd.buffer.is_empty() {
                    is_as_is = false;

                    append_text(text.start(), parent_id, pd, doc);
                    pd.buffer.clear();
                }

                let mut parser = xmlparser::Tokenizer::from(fragment);
                parser.enable_fragment_mode();

                let mut tag_name = TagNameSpan::new_null();
                depth += 1;
                process_tokens(parser, depth, parent_id, &mut tag_name, pd, doc)?;
                pd.buffer.clear();
            }
        }
    }

    if !pd.buffer.is_empty() {
        append_text(text.start(), parent_id, pd, doc);
        pd.buffer.clear();
    }

    Ok(())
}

fn append_text(
    orig_pos: usize,
    parent_id: NodeId,
    pd: &ParserData,
    doc: &mut Document,
) {
    // `unwrap` is safe, because the input text was already a valid UTF-8 string.
    let text = str::from_utf8(&pd.buffer).unwrap();

    match pd.prev_node_type {
        Some(xmlparser::Token::Cdata(_)) |
        Some(xmlparser::Token::Text(_)) |
        Some(xmlparser::Token::Whitespaces(_)) => {
            if let Some(node) = doc.nodes.iter_mut().last() {
                if let NodeKind::Text(ref mut prev_text) = node.kind {
                    prev_text.push_str(text);
                }
            }
        }
        _ => {
            doc.append(parent_id, NodeKind::Text(text.to_string()), orig_pos);
        }
    }
}

enum NextChunk<'a> {
    Byte(u8),
    Char(char),
    Text(StrSpan<'a>),
}

fn parse_next_chunk<'a>(
    s: &mut Stream<'a>,
    entities: &[Entity<'a>],
) -> Result<NextChunk<'a>, Error> {
    debug_assert!(!s.at_end());

    // `unwrap` is safe, because we already checked that stream is not at the end.
    // But we have an additional `debug_assert` above just in case.
    let c = s.curr_byte().unwrap();

    // Check for character/entity references.
    if c == b'&' {
        match s.try_consume_reference() {
            Some(Reference::CharRef(ch)) => {
                Ok(NextChunk::Char(ch))
            }
            Some(Reference::EntityRef(name)) => {
                match entities.iter().find(|e| e.name == name) {
                    Some(entity) => {
                        Ok(NextChunk::Text(entity.value))
                    }
                    None => {
                        let pos = s.gen_error_pos();
                        Err(Error::UnknownEntityReference(name.into(), pos))
                    }
                }
            }
            None => {
                s.advance(1);
                Ok(NextChunk::Byte(c))
            }
        }
    } else {
        s.advance(1);
        Ok(NextChunk::Byte(c))
    }
}

fn process_cdata<'d>(
    cdata: StrSpan<'d>,
    parent_id: NodeId,
    pd: &ParserData,
    doc: &mut Document<'d>,
) {
    match pd.prev_node_type {
        Some(xmlparser::Token::Text(_)) | Some(xmlparser::Token::Whitespaces(_)) => {
            if let Some(node) = doc.nodes.iter_mut().last() {
                if let NodeKind::Text(ref mut text) = node.kind {
                    text.push_str(cdata.to_str());
                }
            }
        }
        _ => {
            doc.append(parent_id, NodeKind::Text(cdata.to_str().to_owned()), cdata.start());
        }
    }
}

// https://www.w3.org/TR/REC-xml/#AVNormalize
fn normalize_attribute<'d>(
    depth: u8,
    text: StrSpan<'d>,
    entities: &[Entity],
    buffer: &mut Vec<u8>,
) -> Result<Cow<'d, str>, Error> {
    if is_normalization_required(&text) {
        buffer.clear();
        _normalize_attribute(text, entities, depth, buffer)?;
        // `unwrap` is safe, because buffer must contain a valid UTF-8 string.
        Ok(Cow::Owned(str::from_utf8(buffer).unwrap().to_owned()))
    } else {
        Ok(Cow::Borrowed(text.to_str()))
    }
}

fn is_normalization_required(text: &StrSpan) -> bool {
    // We assume that `&` indicates an entity or a character reference.
    // But in rare cases it can be just an another character.

    fn check(c: u8) -> bool {
        match c {
              b'&'
            | b'\t'
            | b'\n'
            | b'\r' => true,
            _ => false,
        }
    }

    text.to_str().bytes().any(check)
}

fn _normalize_attribute(
    text: StrSpan,
    entities: &[Entity],
    depth: u8,
    buffer: &mut Vec<u8>,
) -> Result<(), Error> {
    let mut depth = depth;

    let mut s = Stream::from(text);
    while !s.at_end() {
        // Safe, because we already checked that stream is not at the end.
        let c = s.curr_byte_unchecked();

        // Check for character/entity references.
        if c == b'&' {
            match s.try_consume_reference() {
                Some(Reference::CharRef(ch)) => {
                    for b in CharToBytes::new(ch) {
                        if depth > 0 {
                            push_byte_to_attr(b, None, buffer);
                        } else {
                            // Characters not from entity should be added as is.
                            // Not sure why... At least `lxml` produces the same results.
                            buffer.push(b);
                        }
                    }

                    continue;
                }
                Some(Reference::EntityRef(name)) => {
                    if depth > ENTITY_DEPTH {
                        let pos = s.gen_error_pos();
                        return Err(Error::EntityReferenceLoop(pos));
                    }

                    match entities.iter().find(|e| e.name == name) {
                        Some(entity) => {
                            depth += 1;
                            _normalize_attribute(entity.value, entities, depth, buffer)?;
                        }
                        None => {
                            let pos = s.gen_error_pos();
                            return Err(Error::UnknownEntityReference(name.into(), pos));
                        }
                    }

                    continue;
                }
                None => {
                    s.advance(1);
                }
            }
        } else {
            s.advance(1);
        }

        push_byte_to_attr(c, s.get_curr_byte(), buffer);
    }

    Ok(())
}

fn push_byte_to_attr(mut c: u8, c2: Option<u8>, buf: &mut Vec<u8>) {
    // \r in \r\n should be ignored.
    if c == b'\r' && c2 == Some(b'\n') {
        return;
    }

    // \n, \r and \t should be converted into spaces.
    c = match c {
        b'\n' | b'\r' | b'\t' => b' ',
        _ => c,
    };

    buf.push(c);
}

// Translate \r\n and any \r that is not followed by \n into a single \n character.
//
// https://www.w3.org/TR/xml/#sec-line-ends
fn push_byte_to_text(c: u8, at_end: bool, buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\r') {
        let idx = buf.len() - 1;
        buf[idx] = b'\n';

        if at_end && c == b'\r' {
            buf.push(b'\n');
        } else if c != b'\n' {
            buf.push(c);
        }
    } else if at_end && c == b'\r' {
        buf.push(b'\n');
    } else {
        buf.push(c);
    }
}

fn gen_qname_string(prefix: &str, local: &str) -> String {
    if prefix.is_empty() {
        local.to_string()
    } else {
        format!("{}:{}", prefix, local)
    }
}

fn err_pos_from_span(text: StrSpan) -> TextPos {
    Stream::from(text).gen_error_pos()
}

fn err_pos_from_qname(prefix: StrSpan, local: StrSpan) -> TextPos {
    let err_span = if prefix.is_empty() { local } else { prefix };
    err_pos_from_span(err_span)
}

fn err_pos_from_tag_name(prefix: StrSpan, local: StrSpan, close_tag: bool) -> TextPos {
    let mut pos = err_pos_from_qname(prefix, local);
    // jump before '<'
    pos.col -= if close_tag { 2 } else { 1 };
    pos
}

fn orig_pos_from_tag_name(tag_name: &TagNameSpan) -> usize {
    let span = if tag_name.prefix.is_empty() { tag_name.name } else { tag_name.prefix };
    span.start() - 1 // jump before '<'
}


/// Iterate over `char` by `u8`.
struct CharToBytes {
    buf: [u8; 4],
    idx: u8,
}

impl CharToBytes {
    fn new(c: char) -> Self {
        let mut buf = [0xFF; 4];
        c.encode_utf8(&mut buf);

        CharToBytes {
            buf,
            idx: 0,
        }
    }
}

impl Iterator for CharToBytes {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx < 4 {
            let b = self.buf[self.idx as usize];

            if b != 0xFF {
                self.idx += 1;
                return Some(b);
            } else {
                self.idx = 4;
            }
        }

        None
    }
}
