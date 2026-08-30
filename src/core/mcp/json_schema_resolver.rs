use super::json_value::Value;

const SYNTHETIC_ROOT_URI: &str = "fx-schema:/root";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Draft202012,
    Draft7,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_uri_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidSchema,
    SchemaLimitExceeded,
    ExternalReference,
    UnresolvedReference,
    OutOfMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorKind {
    Static,
    Dynamic,
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub schema: Value,
    pub resource_index: usize,
    pub anchor_kind: Option<AnchorKind>,
    pub anchor_name: Option<String>,
}

struct Resource {
    uri: String,
    schema: Value,
}

struct Anchor {
    resource_index: usize,
    name: String,
    schema: Value,
    kind: AnchorKind,
}

pub struct Resolver {
    dialect: Dialect,
    limits: Limits,
    resources: Vec<Resource>,
    anchors: Vec<Anchor>,
    indexed_nodes: usize,
}

impl Resolver {
    pub fn init(root: Value, dialect: Dialect, limits: Limits) -> Result<Resolver, Error> {
        let mut resolver = Resolver {
            dialect,
            limits,
            resources: Vec::new(),
            anchors: Vec::new(),
            indexed_nodes: 0,
        };

        let root_uri = if let Some(id) = resolver.schema_id(&root) {
            let id = id.to_string();
            resolver.resolve_identifier(SYNTHETIC_ROOT_URI, &id)?
        } else {
            SYNTHETIC_ROOT_URI.to_string()
        };

        resolver.resources.push(Resource {
            uri: root_uri,
            schema: root,
        });
        let root_schema = resolver.resources[0].schema.clone();
        resolver.index_schema(&root_schema, 0, 0, true)?;
        Ok(resolver)
    }

    pub fn root_resource(&self) -> usize {
        0
    }

    pub fn enter_resource(
        &self,
        schema: &Value,
        inherited_resource: usize,
    ) -> Result<usize, Error> {
        if !schema.is_object()
            || self.schema_id(schema).is_none()
            || (self.dialect == Dialect::Draft7 && schema.as_object().unwrap().get("$ref").is_some())
        {
            return Ok(inherited_resource);
        }
        for (index, resource) in self.resources.iter().enumerate() {
            if same_object(schema, &resource.schema) {
                return Ok(index);
            }
        }
        if self.dialect == Dialect::Draft7 && has_plain_name_identifier(schema) {
            return Ok(inherited_resource);
        }
        Err(Error::InvalidSchema)
    }

    pub fn resolve(&self, current_resource: usize, reference: &str) -> Result<Resolved, Error> {
        if current_resource >= self.resources.len() {
            return Err(Error::InvalidSchema);
        }
        let absolute = resolve_uri(
            &self.resources[current_resource].uri,
            reference,
            self.limits.max_uri_bytes,
        )?;
        let parts = split_uri(&absolute);
        let resource_index = self
            .find_resource(parts.document)
            .ok_or(Error::ExternalReference)?;
        let resource = &self.resources[resource_index];
        if parts.fragment.is_none() || parts.fragment.unwrap().is_empty() {
            return Ok(Resolved {
                schema: resource.schema.clone(),
                resource_index,
                anchor_kind: None,
                anchor_name: None,
            });
        }
        let fragment = parts.fragment.unwrap();
        if fragment.starts_with('/') {
            let schema = resolve_pointer(&resource.schema, fragment, self.limits)?;
            return Ok(Resolved {
                schema,
                resource_index,
                anchor_kind: None,
                anchor_name: None,
            });
        }
        let decoded = percent_decode(fragment.as_bytes())?;
        for anchor in &self.anchors {
            if anchor.resource_index == resource_index && anchor.name.as_bytes() == decoded.as_slice()
            {
                return Ok(Resolved {
                    schema: anchor.schema.clone(),
                    resource_index,
                    anchor_kind: Some(anchor.kind),
                    anchor_name: Some(anchor.name.clone()),
                });
            }
        }
        Err(Error::UnresolvedReference)
    }

    pub fn dynamic_anchor(&self, resource_index: usize, name: &str) -> Option<Value> {
        for anchor in &self.anchors {
            if anchor.resource_index == resource_index
                && anchor.kind == AnchorKind::Dynamic
                && anchor.name == name
            {
                return Some(anchor.schema.clone());
            }
        }
        None
    }

    fn index_schema(
        &mut self,
        schema: &Value,
        inherited_resource: usize,
        depth: usize,
        is_resource_root: bool,
    ) -> Result<(), Error> {
        if depth > self.limits.max_depth {
            return Err(Error::SchemaLimitExceeded);
        }
        self.indexed_nodes = self
            .indexed_nodes
            .checked_add(1)
            .ok_or(Error::SchemaLimitExceeded)?;
        if self.indexed_nodes > self.limits.max_nodes {
            return Err(Error::SchemaLimitExceeded);
        }
        if !schema.is_object() {
            return Ok(());
        }
        let object = schema.as_object().unwrap();

        if self.dialect == Dialect::Draft7 && object.get("$ref").is_some() {
            return Ok(());
        }

        let mut resource_index = inherited_resource;
        if !is_resource_root {
            if let Some(id) = self.schema_id(schema) {
                let id = id.to_string();
                let base = self.resources[inherited_resource].uri.clone();
                let uri = self.resolve_identifier(&base, &id)?;
                if uri == self.resources[inherited_resource].uri
                    && self.dialect == Dialect::Draft7
                    && has_plain_name_identifier(schema)
                {
                    // Plain-name identifier scoped to the inherited resource.
                } else {
                    if self.find_resource(&uri).is_some() {
                        return Err(Error::InvalidSchema);
                    }
                    self.resources.push(Resource {
                        uri,
                        schema: schema.clone(),
                    });
                    resource_index = self.resources.len() - 1;
                }
            }
        }

        match self.dialect {
            Dialect::Draft202012 => {
                self.index_anchor(schema, resource_index, "$anchor", AnchorKind::Static)?;
                self.index_anchor(schema, resource_index, "$dynamicAnchor", AnchorKind::Dynamic)?;
            }
            Dialect::Draft7 => self.index_identifier_anchor(schema, resource_index)?,
        }

        for (keyword, value) in object.iter() {
            if !keyword_contains_schemas(self.dialect, keyword) {
                continue;
            }
            match value {
                Value::Object(_) => {
                    if schema_map_keyword(self.dialect, keyword) {
                        for (_, child_value) in value.as_object().unwrap().iter() {
                            if self.dialect == Dialect::Draft7
                                && keyword == "dependencies"
                                && child_value.is_array()
                            {
                                continue;
                            }
                            self.index_schema(child_value, resource_index, depth + 1, false)?;
                        }
                    } else {
                        self.index_schema(value, resource_index, depth + 1, false)?;
                    }
                }
                Value::Array(array) => {
                    for child in array {
                        self.index_schema(child, resource_index, depth + 1, false)?;
                    }
                }
                Value::Bool(_) => {
                    self.index_schema(value, resource_index, depth + 1, false)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn index_anchor(
        &mut self,
        schema: &Value,
        resource_index: usize,
        keyword: &str,
        kind: AnchorKind,
    ) -> Result<(), Error> {
        let object = match schema.as_object() {
            Some(object) => object,
            None => return Ok(()),
        };
        let value = match object.get(keyword) {
            Some(value) => value,
            None => return Ok(()),
        };
        let name = match value {
            Value::String(name) => name,
            _ => return Err(Error::InvalidSchema),
        };
        if !valid_anchor_name(name) {
            return Err(Error::InvalidSchema);
        }
        for anchor in &self.anchors {
            if anchor.resource_index == resource_index && anchor.name.as_str() == name.as_str() {
                return Err(Error::InvalidSchema);
            }
        }
        self.anchors.push(Anchor {
            resource_index,
            name: name.clone(),
            schema: schema.clone(),
            kind,
        });
        Ok(())
    }

    fn index_identifier_anchor(
        &mut self,
        schema: &Value,
        resource_index: usize,
    ) -> Result<(), Error> {
        let id = match self.schema_id(schema) {
            Some(id) => id,
            None => return Ok(()),
        };
        let hash = match id.find('#') {
            Some(hash) => hash,
            None => return Ok(()),
        };
        let name = &id[hash + 1..];
        if name.is_empty() {
            return Ok(());
        }
        if !valid_anchor_name(name) {
            return Err(Error::InvalidSchema);
        }
        for anchor in &self.anchors {
            if anchor.resource_index == resource_index && anchor.name.as_str() == name {
                return Err(Error::InvalidSchema);
            }
        }
        self.anchors.push(Anchor {
            resource_index,
            name: name.to_string(),
            schema: schema.clone(),
            kind: AnchorKind::Static,
        });
        Ok(())
    }

    fn resolve_identifier(&self, base: &str, identifier: &str) -> Result<String, Error> {
        let absolute = resolve_uri(base, identifier, self.limits.max_uri_bytes)?;
        let parts = split_uri(&absolute);
        if let Some(fragment) = parts.fragment {
            if !fragment.is_empty() {
                if self.dialect != Dialect::Draft7 || !valid_anchor_name(fragment) {
                    return Err(Error::InvalidSchema);
                }
            }
        }
        if parts.document.len() == absolute.len() {
            return Ok(absolute);
        }
        Ok(parts.document.to_string())
    }

    fn find_resource(&self, uri: &str) -> Option<usize> {
        for (index, resource) in self.resources.iter().enumerate() {
            if resource.uri == uri {
                return Some(index);
            }
        }
        None
    }

    fn schema_id<'a>(&self, schema: &'a Value) -> Option<&'a str> {
        let object = schema.as_object()?;
        if self.dialect == Dialect::Draft7 && object.get("$ref").is_some() {
            return None;
        }
        match object.get("$id")? {
            Value::String(id) => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct UriParts<'a> {
    document: &'a str,
    fragment: Option<&'a str>,
}

fn split_uri(uri: &str) -> UriParts<'_> {
    match uri.find('#') {
        None => UriParts {
            document: uri,
            fragment: None,
        },
        Some(hash) => UriParts {
            document: &uri[..hash],
            fragment: Some(&uri[hash + 1..]),
        },
    }
}

fn resolve_uri(base_text: &str, reference: &str, max_uri_bytes: usize) -> Result<String, Error> {
    if base_text.len() > max_uri_bytes || reference.len() > max_uri_bytes {
        return Err(Error::SchemaLimitExceeded);
    }
    let base = UriRef::parse(base_text).map_err(|_| Error::InvalidSchema)?;
    let reference = UriRef::parse(reference).map_err(|_| Error::InvalidSchema)?;
    let resolved = base.resolve(&reference);
    let rendered = resolved.render();
    if rendered.len() > max_uri_bytes {
        return Err(Error::SchemaLimitExceeded);
    }
    Ok(rendered)
}

#[derive(Clone, Debug)]
struct UriRef {
    scheme: Option<String>,
    authority: Option<String>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

impl UriRef {
    fn parse(input: &str) -> Result<UriRef, ()> {
        let mut rest = input;

        let scheme = if let Some(colon) = find_scheme_colon(input) {
            let scheme = &input[..colon];
            let mut bytes = scheme.bytes();
            if scheme.is_empty() || !bytes.next().unwrap().is_ascii_alphabetic() {
                return Err(());
            }
            if !scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
            {
                return Err(());
            }
            rest = &input[colon + 1..];
            Some(scheme.to_string())
        } else {
            None
        };

        let authority = if let Some(after) = rest.strip_prefix("//") {
            rest = after;
            let end = rest
                .find(|c| c == '/' || c == '?' || c == '#')
                .unwrap_or(rest.len());
            let authority = &rest[..end];
            rest = &rest[end..];
            Some(authority.to_string())
        } else {
            None
        };

        let (before_fragment, fragment) = match rest.find('#') {
            Some(hash) => (&rest[..hash], Some(rest[hash + 1..].to_string())),
            None => (rest, None),
        };
        let (before_query, query) = match before_fragment.find('?') {
            Some(q) => (&before_fragment[..q], Some(before_fragment[q + 1..].to_string())),
            None => (before_fragment, None),
        };

        Ok(UriRef {
            scheme,
            authority,
            path: before_query.to_string(),
            query,
            fragment,
        })
    }

    fn resolve(&self, reference: &UriRef) -> UriRef {
        let mut target = UriRef {
            scheme: None,
            authority: None,
            path: String::new(),
            query: None,
            fragment: None,
        };

        if reference.scheme.is_some() {
            target.scheme = reference.scheme.clone();
            target.authority = reference.authority.clone();
            target.path = remove_dot_segments(&reference.path);
            target.query = reference.query.clone();
        } else {
            target.scheme = self.scheme.clone();
            if reference.authority.is_some() {
                target.authority = reference.authority.clone();
                target.path = remove_dot_segments(&reference.path);
                target.query = reference.query.clone();
            } else {
                target.authority = self.authority.clone();
                if reference.path.is_empty() {
                    target.path = self.path.clone();
                    target.query = if reference.query.is_some() {
                        reference.query.clone()
                    } else {
                        self.query.clone()
                    };
                } else {
                    if reference.path.starts_with('/') {
                        target.path = remove_dot_segments(&reference.path);
                    } else {
                        let merged = if self.authority.is_some() && self.path.is_empty() {
                            format!("/{}", reference.path)
                        } else if let Some((directory, _)) = self.path.rsplit_once('/') {
                            format!("{}/{}", directory, reference.path)
                        } else {
                            reference.path.clone()
                        };
                        target.path = remove_dot_segments(&merged);
                    }
                    target.query = reference.query.clone();
                }
            }
        }
        target.fragment = reference.fragment.clone();
        target
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(scheme) = &self.scheme {
            out.push_str(scheme);
            out.push(':');
        }
        if let Some(authority) = &self.authority {
            out.push_str("//");
            out.push_str(authority);
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        if let Some(fragment) = &self.fragment {
            out.push('#');
            out.push_str(fragment);
        }
        out
    }
}

fn find_scheme_colon(input: &str) -> Option<usize> {
    for (index, byte) in input.bytes().enumerate() {
        match byte {
            b':' => return Some(index),
            b'/' | b'?' | b'#' => return None,
            _ => {}
        }
    }
    None
}

fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output: Vec<&str> = Vec::new();
    while !input.is_empty() {
        if input.starts_with("../") {
            input = &input[3..];
        } else if input.starts_with("./") {
            input = &input[2..];
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") {
            input = &input[3..];
            output.pop();
        } else if input == "/.." {
            input = "/";
            output.pop();
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let (segment, rest) = split_first_segment(input);
            output.push(segment);
            input = rest;
        }
    }
    let mut result = String::new();
    for segment in output {
        result.push_str(segment);
    }
    result
}

fn split_first_segment(input: &str) -> (&str, &str) {
    let start = if input.starts_with('/') { 1 } else { 0 };
    match input[start..].find('/') {
        Some(index) => (&input[..start + index], &input[start + index..]),
        None => (input, ""),
    }
}

fn resolve_pointer(resource: &Value, fragment: &str, limits: Limits) -> Result<Value, Error> {
    let mut current = resource;
    let mut count = 0usize;
    for raw_segment in fragment[1..].split('/') {
        count = count.checked_add(1).ok_or(Error::SchemaLimitExceeded)?;
        if count > limits.max_depth {
            return Err(Error::SchemaLimitExceeded);
        }
        let decoded_percent = percent_decode(raw_segment.as_bytes())?;
        let segment = decode_pointer_segment(&decoded_percent)?;
        current = match current {
            Value::Object(object) => object.get(&segment).ok_or(Error::UnresolvedReference)?,
            Value::Array(array) => {
                if segment.len() > 1 && segment.as_bytes()[0] == b'0' {
                    return Err(Error::UnresolvedReference);
                }
                let index = segment
                    .parse::<usize>()
                    .map_err(|_| Error::UnresolvedReference)?;
                if index >= array.len() {
                    return Err(Error::UnresolvedReference);
                }
                &array[index]
            }
            _ => return Err(Error::UnresolvedReference),
        };
    }
    Ok(current.clone())
}

fn percent_decode(raw: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    while index < raw.len() {
        if raw[index] == b'%' {
            if index + 2 >= raw.len() {
                return Err(Error::UnresolvedReference);
            }
            let hex = std::str::from_utf8(&raw[index + 1..index + 3])
                .map_err(|_| Error::UnresolvedReference)?;
            let byte = u8::from_str_radix(hex, 16).map_err(|_| Error::UnresolvedReference)?;
            out.push(byte);
            index += 3;
        } else {
            out.push(raw[index]);
            index += 1;
        }
    }
    Ok(out)
}

fn decode_pointer_segment(raw: &[u8]) -> Result<String, Error> {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    while index < raw.len() {
        if raw[index] != b'~' {
            out.push(raw[index]);
            index += 1;
            continue;
        }
        if index + 1 >= raw.len() {
            return Err(Error::UnresolvedReference);
        }
        match raw[index + 1] {
            b'0' => out.push(b'~'),
            b'1' => out.push(b'/'),
            _ => return Err(Error::UnresolvedReference),
        }
        index += 2;
    }
    String::from_utf8(out).map_err(|_| Error::UnresolvedReference)
}

fn has_plain_name_identifier(schema: &Value) -> bool {
    let object = match schema.as_object() {
        Some(object) => object,
        None => return false,
    };
    let id = match object.get("$id") {
        Some(Value::String(id)) => id,
        _ => return false,
    };
    match id.find('#') {
        Some(hash) => !id[hash + 1..].is_empty(),
        None => false,
    }
}

fn same_object(left: &Value, right: &Value) -> bool {
    let left_object = match left.as_object() {
        Some(object) => object,
        None => return false,
    };
    let right_object = match right.as_object() {
        Some(object) => object,
        None => return false,
    };
    if left_object.is_empty() || right_object.is_empty() {
        return false;
    }
    left_object.id() == right_object.id()
}

fn valid_anchor_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let first = match bytes.next() {
        Some(first) => first,
        None => return false,
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    name.bytes()
        .skip(1)
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.' || byte == b':')
}

fn keyword_contains_schema_map(dialect: Dialect, keyword: &str) -> bool {
    match dialect {
        Dialect::Draft202012 => {
            keyword == "$defs"
                || keyword == "properties"
                || keyword == "patternProperties"
                || keyword == "dependentSchemas"
        }
        Dialect::Draft7 => {
            keyword == "definitions"
                || keyword == "properties"
                || keyword == "patternProperties"
                || keyword == "dependencies"
        }
    }
}

fn keyword_contains_schemas(dialect: Dialect, keyword: &str) -> bool {
    if keyword_contains_schema_map(dialect, keyword) {
        return true;
    }
    if dialect == Dialect::Draft202012
        && (keyword == "prefixItems"
            || keyword == "unevaluatedItems"
            || keyword == "unevaluatedProperties")
    {
        return true;
    }
    if dialect == Dialect::Draft7 && keyword == "additionalItems" {
        return true;
    }
    keyword == "items"
        || keyword == "contains"
        || keyword == "additionalProperties"
        || keyword == "propertyNames"
        || keyword == "if"
        || keyword == "then"
        || keyword == "else"
        || keyword == "allOf"
        || keyword == "anyOf"
        || keyword == "oneOf"
        || keyword == "not"
        || (dialect == Dialect::Draft202012 && keyword == "contentSchema")
}

fn schema_map_keyword(dialect: Dialect, keyword: &str) -> bool {
    keyword_contains_schema_map(dialect, keyword)
}

#[cfg(test)]
mod tests {
    use super::super::json_value::parse_json;
    use super::{same_object, AnchorKind, Dialect, Limits, Resolver};

    fn limits() -> Limits {
        Limits {
            max_depth: 64,
            max_nodes: 1024,
            max_uri_bytes: 64 * 1024,
        }
    }

    #[test]
    fn resolver_scopes_anchors_to_resources_and_resolves_rfc_3986_relative_identifiers() {
        let schema_json = r#"{
          "$id":"https://example.test/schemas/root.json",
          "$anchor":"value",
          "$defs":{
            "inner":{"$id":"nested/inner.json","$anchor":"value","type":"integer"},
            "other":{"$id":"nested/dir/other.json","$ref":"../inner.json#value"}
          }
        }"#;
        let parsed = parse_json(schema_json).unwrap();
        let resolver = Resolver::init(parsed, Dialect::Draft202012, limits()).unwrap();

        let other = resolver.resolve(0, "nested/dir/other.json").unwrap();
        let inner = resolver
            .resolve(other.resource_index, "../inner.json#value")
            .unwrap();
        assert_eq!(AnchorKind::Static, inner.anchor_kind.unwrap());
        assert!(inner.schema.as_object().unwrap().get("type").is_some());
        let root_anchor = resolver.resolve(0, "#value").unwrap();
        assert!(!same_object(&root_anchor.schema, &inner.schema));
    }

    #[test]
    fn resolver_rejects_unregistered_external_documents_without_io() {
        let parsed = parse_json(r#"{"$id":"https://example.test/root"}"#).unwrap();
        let resolver = Resolver::init(parsed, Dialect::Draft202012, limits()).unwrap();
        assert!(matches!(
            resolver.resolve(0, "https://example.test/external"),
            Err(super::Error::ExternalReference)
        ));
    }
}
