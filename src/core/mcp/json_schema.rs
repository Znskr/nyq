use super::json_number::{self, Number};
use super::json_schema_pattern;
use super::json_schema_resolver::{self, AnchorKind, Dialect, Resolver};
use super::json_value::{parse_json, JsonMap, Value};
use std::cmp::Ordering;
use std::collections::HashSet;

type StdResult<T> = std::result::Result<T, Error>;

const DIALECT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";
const DIALECT_DRAFT_7: &str = "http://json-schema.org/draft-07/schema";

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_schema_bytes: usize,
    pub max_instance_bytes: usize,
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_steps: usize,
    pub max_ref_hops: usize,
    pub max_container_entries: usize,
    pub max_number_bytes: usize,
    pub max_number_exponent_abs: i64,
    pub max_number_expanded_digits: usize,
    pub max_pattern_states: usize,
    pub max_pattern_repeat: usize,
    pub max_pattern_steps: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_schema_bytes: 256 * 1024,
            max_instance_bytes: 1024 * 1024,
            max_depth: 64,
            max_nodes: 4096,
            max_steps: 100_000,
            max_ref_hops: 256,
            max_container_entries: 4096,
            max_number_bytes: 4096,
            max_number_exponent_abs: 1_000_000,
            max_number_expanded_digits: 8192,
            max_pattern_states: 2048,
            max_pattern_repeat: 1024,
            max_pattern_steps: 200_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidSchema,
    UnsupportedDialect,
    ExternalReference,
    UnresolvedReference,
    SchemaLimitExceeded,
    InstanceLimitExceeded,
    InvalidJson,
    OutOfMemory,
}

impl From<json_schema_resolver::Error> for Error {
    fn from(error: json_schema_resolver::Error) -> Self {
        map_resolver_err(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaAssessment {
    LocallyEvaluable,
    ServerAuthoritative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Violation {
    Type,
    Constant,
    Enumeration,
    MultipleOf,
    Maximum,
    ExclusiveMaximum,
    Minimum,
    ExclusiveMinimum,
    MinLength,
    MaxLength,
    Pattern,
    MinItems,
    MaxItems,
    UniqueItems,
    AdditionalItems,
    Contains,
    UnevaluatedItems,
    MinProperties,
    MaxProperties,
    Required,
    DependentRequired,
    Properties,
    PatternProperties,
    AdditionalProperties,
    PropertyNames,
    DependentSchemas,
    Dependencies,
    UnevaluatedProperties,
    AllOf,
    AnyOf,
    OneOf,
    Not,
    Conditional,
    Reference,
}

impl Violation {
    pub fn label(&self) -> &'static str {
        match self {
            Violation::Type => "type",
            Violation::Constant => "const",
            Violation::Enumeration => "enum",
            Violation::MultipleOf => "multipleOf",
            Violation::Maximum => "maximum",
            Violation::ExclusiveMaximum => "exclusiveMaximum",
            Violation::Minimum => "minimum",
            Violation::ExclusiveMinimum => "exclusiveMinimum",
            Violation::MinLength => "minLength",
            Violation::MaxLength => "maxLength",
            Violation::Pattern => "pattern",
            Violation::MinItems => "minItems",
            Violation::MaxItems => "maxItems",
            Violation::UniqueItems => "uniqueItems",
            Violation::AdditionalItems => "additionalItems",
            Violation::Contains => "contains",
            Violation::UnevaluatedItems => "unevaluatedItems",
            Violation::MinProperties => "minProperties",
            Violation::MaxProperties => "maxProperties",
            Violation::Required => "required",
            Violation::DependentRequired => "dependentRequired",
            Violation::Properties => "properties",
            Violation::PatternProperties => "patternProperties",
            Violation::AdditionalProperties => "additionalProperties",
            Violation::PropertyNames => "propertyNames",
            Violation::DependentSchemas => "dependentSchemas",
            Violation::Dependencies => "dependencies",
            Violation::UnevaluatedProperties => "unevaluatedProperties",
            Violation::AllOf => "allOf",
            Violation::AnyOf => "anyOf",
            Violation::OneOf => "oneOf",
            Violation::Not => "not",
            Violation::Conditional => "if/then/else",
            Violation::Reference => "$ref",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Result {
    Valid,
    Invalid(Violation),
    ServerAuthoritative,
}

fn detect_dialect(schema: &Value) -> StdResult<Dialect> {
    if !schema.is_object() {
        return Ok(Dialect::Draft202012);
    }
    let declaration = match schema.as_object().unwrap().get("$schema") {
        Some(value) => value,
        None => return Ok(Dialect::Draft202012),
    };
    let uri = string_value(declaration).ok_or(Error::InvalidSchema)?;
    dialect_from_uri(uri)
}

fn dialect_from_uri(uri: &str) -> StdResult<Dialect> {
    if uri == DIALECT_2020_12 || uri == "https://json-schema.org/draft/2020-12/schema#" {
        return Ok(Dialect::Draft202012);
    }
    if uri == DIALECT_DRAFT_7 || uri == "http://json-schema.org/draft-07/schema#" {
        return Ok(Dialect::Draft7);
    }
    Err(Error::UnsupportedDialect)
}

pub fn validate_schema_text(schema_json: &str, limits: Limits) -> StdResult<SchemaAssessment> {
    if schema_json.len() > limits.max_schema_bytes {
        return Err(Error::SchemaLimitExceeded);
    }
    let parsed = parse_json(schema_json).map_err(|_| Error::InvalidJson)?;
    validate_schema_value(&parsed, limits)
}

pub fn validate_schema_value(schema: &Value, limits: Limits) -> StdResult<SchemaAssessment> {
    let mut remaining = limits.max_nodes;
    check_json_bounds(schema, 0, &mut remaining, limits, BoundKind::Schema)?;
    let dialect = detect_dialect(schema)?;
    let resolver = Resolver::init(schema.clone(), dialect, resolver_limits(limits))
        .map_err(map_resolver_err)?;
    validate_schema_with_resolver(schema, dialect, limits, &resolver)
}

fn validate_schema_with_resolver(
    schema: &Value,
    dialect: Dialect,
    limits: Limits,
    resolver: &Resolver,
) -> StdResult<SchemaAssessment> {
    let mut ctx = SchemaContext {
        dialect,
        resolver,
        limits,
        nodes: 0,
        assessment: SchemaAssessment::LocallyEvaluable,
        reference_objects: HashSet::new(),
    };
    ctx.scan(schema, 0)?;
    ctx.nodes = 0;
    ctx.mark_reference_object(schema)?;
    ctx.check_references(schema, resolver.root_resource(), 0)?;
    Ok(ctx.assessment)
}

pub fn validate_text(
    schema_json: &str,
    instance_json: &str,
    limits: Limits,
) -> StdResult<Result> {
    if schema_json.len() > limits.max_schema_bytes {
        return Err(Error::SchemaLimitExceeded);
    }
    if instance_json.len() > limits.max_instance_bytes {
        return Err(Error::InstanceLimitExceeded);
    }
    let schema_parsed = parse_json(schema_json).map_err(|_| Error::InvalidJson)?;
    let instance_parsed = parse_json(instance_json).map_err(|_| Error::InvalidJson)?;
    validate_value(&schema_parsed, &instance_parsed, limits)
}

pub fn validate_value(schema: &Value, instance: &Value, limits: Limits) -> StdResult<Result> {
    let mut schema_remaining = limits.max_nodes;
    check_json_bounds(schema, 0, &mut schema_remaining, limits, BoundKind::Schema)?;
    let dialect = detect_dialect(schema)?;
    let resolver = Resolver::init(schema.clone(), dialect, resolver_limits(limits))
        .map_err(map_resolver_err)?;
    let schema_assessment = validate_schema_with_resolver(schema, dialect, limits, &resolver)?;

    let mut remaining = limits.max_nodes;
    check_json_bounds(instance, 0, &mut remaining, limits, BoundKind::Instance)?;
    if schema_assessment == SchemaAssessment::ServerAuthoritative {
        return Ok(Result::ServerAuthoritative);
    }

    let mut ctx = ValidationContext {
        dialect,
        resolver: &resolver,
        limits,
        steps: 0,
        pattern_steps: 0,
        ref_hops: 0,
        dynamic_scope: Vec::new(),
    };
    let violation = ctx.validate_at(schema, instance, 0, resolver.root_resource())?;
    Ok(if let Some(violation) = violation {
        Result::Invalid(violation)
    } else {
        Result::Valid
    })
}

fn resolver_limits(limits: Limits) -> json_schema_resolver::Limits {
    json_schema_resolver::Limits {
        max_depth: limits.max_depth,
        max_nodes: limits.max_nodes,
        max_uri_bytes: limits.max_schema_bytes,
    }
}

pub fn require_object_root(schema: &Value) -> StdResult<()> {
    if !schema.is_object() {
        return Err(Error::InvalidSchema);
    }
    let type_value = schema
        .as_object()
        .unwrap()
        .get("type")
        .ok_or(Error::InvalidSchema)?;
    if !matches!(type_value, Value::String(value) if value == "object") {
        return Err(Error::InvalidSchema);
    }
    Ok(())
}

fn map_resolver_err(error: json_schema_resolver::Error) -> Error {
    match error {
        json_schema_resolver::Error::InvalidSchema => Error::InvalidSchema,
        json_schema_resolver::Error::SchemaLimitExceeded => Error::SchemaLimitExceeded,
        json_schema_resolver::Error::ExternalReference => Error::ExternalReference,
        json_schema_resolver::Error::UnresolvedReference => Error::UnresolvedReference,
        json_schema_resolver::Error::OutOfMemory => Error::OutOfMemory,
    }
}

struct SchemaContext<'a> {
    dialect: Dialect,
    resolver: &'a Resolver,
    limits: Limits,
    nodes: usize,
    assessment: SchemaAssessment,
    reference_objects: HashSet<u64>,
}

impl SchemaContext<'_> {
    fn scan(&mut self, schema: &Value, depth: usize) -> StdResult<()> {
        self.consume_node(depth)?;
        match schema {
            Value::Bool(_) => Ok(()),
            Value::Object(object) => {
                if object.len() > self.limits.max_container_entries {
                    return Err(Error::SchemaLimitExceeded);
                }
                if self.dialect == Dialect::Draft7 {
                    if let Some(reference) = object.get("$ref") {
                        string_value(reference).ok_or(Error::InvalidSchema)?;
                        return Ok(());
                    }
                }
                for (keyword, value) in object.iter() {
                    if !known_keyword(self.dialect, keyword) {
                        continue;
                    }
                    self.scan_keyword(keyword, value, depth)?;
                }
                Ok(())
            }
            _ => Err(Error::InvalidSchema),
        }
    }

    fn scan_keyword(&mut self, keyword: &str, value: &Value, depth: usize) -> StdResult<()> {
        match keyword {
            "$schema" => {
                let dialect = string_value(value).ok_or(Error::InvalidSchema)?;
                if dialect_from_uri(dialect)? != self.dialect {
                    return Err(Error::UnsupportedDialect);
                }
                Ok(())
            }
            "$id" | "$anchor" | "$ref" | "$comment" | "title" | "description" | "format"
            | "contentEncoding" | "contentMediaType" => {
                string_value(value).ok_or(Error::InvalidSchema)?;
                Ok(())
            }
            "$dynamicAnchor" | "$dynamicRef" if self.dialect == Dialect::Draft202012 => {
                string_value(value).ok_or(Error::InvalidSchema)?;
                Ok(())
            }
            "type" => validate_type_keyword(value),
            "pattern" => {
                let source = string_value(value).ok_or(Error::InvalidSchema)?;
                self.assessment = combine_assessment(
                    self.assessment,
                    assess_pattern(source.as_bytes(), self.limits)?,
                );
                Ok(())
            }
            "enum" | "examples" | "required" => {
                let array = value.as_array().ok_or(Error::InvalidSchema)?;
                if array.len() > self.limits.max_container_entries {
                    return Err(Error::InvalidSchema);
                }
                if keyword == "required" {
                    validate_unique_string_array(value)?;
                }
                Ok(())
            }
            "multipleOf" | "maximum" | "exclusiveMaximum" | "minimum" | "exclusiveMinimum" => {
                let number = schema_number(value, self.limits)?;
                if keyword == "multipleOf" && (number.is_negative() || number.is_zero()) {
                    return Err(Error::InvalidSchema);
                }
                Ok(())
            }
            "maxLength" | "minLength" | "maxItems" | "minItems" | "maxProperties"
            | "minProperties" => {
                non_negative_usize(value, self.limits)?;
                Ok(())
            }
            "maxContains" | "minContains" if self.dialect == Dialect::Draft202012 => {
                non_negative_usize(value, self.limits)?;
                Ok(())
            }
            "uniqueItems" | "deprecated" | "readOnly" | "writeOnly" => {
                if !value.is_bool() {
                    return Err(Error::InvalidSchema);
                }
                Ok(())
            }
            "$vocabulary" if self.dialect == Dialect::Draft202012 => {
                let object = value.as_object().ok_or(Error::InvalidSchema)?;
                for (_, entry_value) in object.iter() {
                    if !entry_value.is_bool() {
                        return Err(Error::InvalidSchema);
                    }
                }
                Ok(())
            }
            "patternProperties" => {
                let object = value.as_object().ok_or(Error::InvalidSchema)?;
                if object.len() > self.limits.max_container_entries {
                    return Err(Error::InvalidSchema);
                }
                for (pattern, child) in object.iter() {
                    self.assessment = combine_assessment(
                        self.assessment,
                        assess_pattern(pattern.as_bytes(), self.limits)?,
                    );
                    self.scan(child, depth + 1)?;
                }
                Ok(())
            }
            "$defs" | "definitions" | "properties" | "dependentSchemas" => {
                let object = value.as_object().ok_or(Error::InvalidSchema)?;
                if object.len() > self.limits.max_container_entries {
                    return Err(Error::InvalidSchema);
                }
                for (_, child) in object.iter() {
                    self.scan(child, depth + 1)?;
                }
                Ok(())
            }
            "dependentRequired" if self.dialect == Dialect::Draft202012 => {
                let object = value.as_object().ok_or(Error::InvalidSchema)?;
                if object.len() > self.limits.max_container_entries {
                    return Err(Error::InvalidSchema);
                }
                for (_, names) in object.iter() {
                    let array = names.as_array().ok_or(Error::InvalidSchema)?;
                    if array.len() > self.limits.max_container_entries {
                        return Err(Error::InvalidSchema);
                    }
                    validate_unique_string_array(names)?;
                }
                Ok(())
            }
            "dependencies" if self.dialect == Dialect::Draft7 => {
                let object = value.as_object().ok_or(Error::InvalidSchema)?;
                if object.len() > self.limits.max_container_entries {
                    return Err(Error::InvalidSchema);
                }
                for (_, dependency) in object.iter() {
                    if dependency.is_array() {
                        if dependency.as_array().unwrap().len() > self.limits.max_container_entries {
                            return Err(Error::InvalidSchema);
                        }
                        validate_unique_string_array(dependency)?;
                    } else {
                        self.scan(dependency, depth + 1)?;
                    }
                }
                Ok(())
            }
            "prefixItems" | "allOf" | "anyOf" | "oneOf" => {
                let array = value.as_array().ok_or(Error::InvalidSchema)?;
                if array.len() > self.limits.max_container_entries {
                    return Err(Error::InvalidSchema);
                }
                if keyword != "prefixItems" && array.is_empty() {
                    return Err(Error::InvalidSchema);
                }
                for child in array {
                    self.scan(child, depth + 1)?;
                }
                Ok(())
            }
            "items" => {
                if self.dialect == Dialect::Draft7 {
                    if let Value::Array(array) = value {
                        if array.len() > self.limits.max_container_entries {
                            return Err(Error::InvalidSchema);
                        }
                        for child in array {
                            self.scan(child, depth + 1)?;
                        }
                        return Ok(());
                    }
                }
                self.scan(value, depth + 1)
            }
            "contains" | "additionalItems" | "additionalProperties" | "propertyNames" | "if"
            | "then" | "else" | "not" | "unevaluatedItems" | "unevaluatedProperties"
            | "contentSchema" => self.scan(value, depth + 1),
            _ => Ok(()),
        }
    }

    fn check_references(
        &mut self,
        schema: &Value,
        inherited_resource: usize,
        depth: usize,
    ) -> StdResult<()> {
        self.consume_node(depth)?;
        if !schema.is_object() {
            return Ok(());
        }
        let object = schema.as_object().unwrap();
        let resource = self.resolver.enter_resource(schema, inherited_resource)?;
        if let Some(value) = object.get("$ref") {
            let target = self
                .resolver
                .resolve(resource, string_value(value).unwrap())?;
            self.check_reference_target(&target.schema, target.resource_index, depth + 1)?;
        }
        if self.dialect == Dialect::Draft7 && object.get("$ref").is_some() {
            return Ok(());
        }
        if let Some(value) = object.get("$dynamicRef") {
            let target = self
                .resolver
                .resolve(resource, string_value(value).unwrap())?;
            self.check_reference_target(&target.schema, target.resource_index, depth + 1)?;
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
                            self.check_references(child_value, resource, depth + 1)?;
                        }
                    } else {
                        self.check_references(value, resource, depth + 1)?;
                    }
                }
                Value::Array(array) => {
                    for child in array {
                        self.check_references(child, resource, depth + 1)?;
                    }
                }
                Value::Bool(_) => {
                    self.check_references(value, resource, depth + 1)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn check_reference_target(
        &mut self,
        schema: &Value,
        resource: usize,
        depth: usize,
    ) -> StdResult<()> {
        if self.mark_reference_object(schema)? {
            return Ok(());
        }
        self.scan(schema, depth)?;
        self.check_references(schema, resource, depth)
    }

    fn mark_reference_object(&mut self, schema: &Value) -> StdResult<bool> {
        let object = match schema.as_object() {
            Some(object) => object,
            None => return Ok(false),
        };
        if object.is_empty() {
            return Ok(false);
        }
        Ok(!self.reference_objects.insert(object.id()))
    }

    fn consume_node(&mut self, depth: usize) -> StdResult<()> {
        if depth > self.limits.max_depth {
            return Err(Error::SchemaLimitExceeded);
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(Error::SchemaLimitExceeded)?;
        if self.nodes > self.limits.max_nodes {
            return Err(Error::SchemaLimitExceeded);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum BoundKind {
    Schema,
    Instance,
}

fn check_json_bounds(
    value: &Value,
    depth: usize,
    remaining: &mut usize,
    limits: Limits,
    kind: BoundKind,
) -> StdResult<()> {
    let limit_error = || match kind {
        BoundKind::Schema => Error::SchemaLimitExceeded,
        BoundKind::Instance => Error::InstanceLimitExceeded,
    };
    if depth > limits.max_depth || *remaining == 0 {
        return Err(limit_error());
    }
    *remaining -= 1;
    match value {
        Value::NumberString(number) => {
            if number.len() > limits.max_number_bytes {
                return Err(limit_error());
            }
        }
        Value::Array(array) => {
            if array.len() > limits.max_container_entries {
                return Err(limit_error());
            }
            for child in array {
                check_json_bounds(child, depth + 1, remaining, limits, kind)?;
            }
        }
        Value::Object(object) => {
            if object.len() > limits.max_container_entries {
                return Err(limit_error());
            }
            for (_, child) in object.iter() {
                check_json_bounds(child, depth + 1, remaining, limits, kind)?;
            }
        }
        _ => {}
    }
    Ok(())
}

struct BitSet {
    bits: Vec<u64>,
}

impl BitSet {
    fn new(len: usize) -> Self {
        Self {
            bits: vec![0; len.div_ceil(64)],
        }
    }

    fn set(&mut self, index: usize) {
        self.bits[index / 64] |= 1 << (index % 64);
    }

    fn is_set(&self, index: usize) -> bool {
        self.bits[index / 64] & (1 << (index % 64)) != 0
    }

    fn set_union(&mut self, other: &BitSet) {
        for index in 0..self.bits.len().min(other.bits.len()) {
            self.bits[index] |= other.bits[index];
        }
    }
}

struct Evaluation {
    item_count: usize,
    property_count: usize,
    items: Option<BitSet>,
    properties: Option<BitSet>,
}

impl Evaluation {
    fn init(instance: &Value) -> Self {
        Self {
            item_count: instance.as_array().map_or(0, |array| array.len()),
            property_count: instance.as_object().map_or(0, |object| object.len()),
            items: None,
            properties: None,
        }
    }

    fn mark_item(&mut self, index: usize) {
        if self.items.is_none() && self.item_count != 0 {
            self.items = Some(BitSet::new(self.item_count));
        }
        if let Some(items) = &mut self.items {
            items.set(index);
        }
    }

    fn mark_property(&mut self, index: usize) {
        if self.properties.is_none() && self.property_count != 0 {
            self.properties = Some(BitSet::new(self.property_count));
        }
        if let Some(properties) = &mut self.properties {
            properties.set(index);
        }
    }

    fn item_is_marked(&self, index: usize) -> bool {
        self.items.as_ref().map_or(false, |items| items.is_set(index))
    }

    fn property_is_marked(&self, index: usize) -> bool {
        self.properties
            .as_ref()
            .map_or(false, |properties| properties.is_set(index))
    }

    fn merge(&mut self, other: &Evaluation) {
        if let Some(other_items) = &other.items {
            if self.items.is_none() {
                self.items = Some(BitSet::new(self.item_count));
            }
            if let Some(items) = &mut self.items {
                items.set_union(other_items);
            }
        }
        if let Some(other_properties) = &other.properties {
            if self.properties.is_none() {
                self.properties = Some(BitSet::new(self.property_count));
            }
            if let Some(properties) = &mut self.properties {
                properties.set_union(other_properties);
            }
        }
    }
}

struct ValidationContext<'a> {
    dialect: Dialect,
    resolver: &'a Resolver,
    limits: Limits,
    steps: usize,
    pattern_steps: usize,
    ref_hops: usize,
    dynamic_scope: Vec<usize>,
}

impl ValidationContext<'_> {
    fn validate(
        &mut self,
        schema: &Value,
        instance: &Value,
        depth: usize,
    ) -> StdResult<Option<Violation>> {
        let inherited_resource = if self.dynamic_scope.is_empty() {
            self.resolver.root_resource()
        } else {
            *self.dynamic_scope.last().unwrap()
        };
        self.validate_at(schema, instance, depth, inherited_resource)
    }

    fn validate_collect(
        &mut self,
        schema: &Value,
        instance: &Value,
        depth: usize,
        evaluated: &mut Evaluation,
    ) -> StdResult<Option<Violation>> {
        let inherited_resource = if self.dynamic_scope.is_empty() {
            self.resolver.root_resource()
        } else {
            *self.dynamic_scope.last().unwrap()
        };
        self.validate_collect_at(schema, instance, depth, inherited_resource, evaluated)
    }

    fn validate_collect_at(
        &mut self,
        schema: &Value,
        instance: &Value,
        depth: usize,
        inherited_resource: usize,
        evaluated: &mut Evaluation,
    ) -> StdResult<Option<Violation>> {
        let mut nested = Evaluation::init(instance);
        let violation =
            self.validate_at_collect(schema, instance, depth, inherited_resource, &mut nested)?;
        if violation.is_none() {
            evaluated.merge(&nested);
        }
        Ok(violation)
    }

    fn validate_at(
        &mut self,
        schema: &Value,
        instance: &Value,
        depth: usize,
        inherited_resource: usize,
    ) -> StdResult<Option<Violation>> {
        let mut evaluated = Evaluation::init(instance);
        self.validate_at_collect(schema, instance, depth, inherited_resource, &mut evaluated)
    }

    fn validate_at_collect(
        &mut self,
        schema: &Value,
        instance: &Value,
        depth: usize,
        inherited_resource: usize,
        evaluated: &mut Evaluation,
    ) -> StdResult<Option<Violation>> {
        self.consume_step(depth)?;
        if let Value::Bool(value) = schema {
            return Ok(if *value { None } else { Some(Violation::Not) });
        }
        let object = schema.as_object().unwrap();
        let resource = self.resolver.enter_resource(schema, inherited_resource)?;
        let pushed = self.dynamic_scope.is_empty()
            || self.dynamic_scope.last().copied() != Some(resource);
        if pushed {
            self.dynamic_scope.push(resource);
        }
        let result = self.validate_at_collect_inner(object, instance, depth, resource, evaluated);
        if pushed {
            self.dynamic_scope.pop();
        }
        result
    }

    fn validate_at_collect_inner(
        &mut self,
        object: &JsonMap,
        instance: &Value,
        depth: usize,
        resource: usize,
        evaluated: &mut Evaluation,
    ) -> StdResult<Option<Violation>> {
        if let Some(reference) = object.get("$ref") {
            self.consume_reference()?;
            let target = self
                .resolver
                .resolve(resource, string_value(reference).unwrap())?;
            let violation = self.validate_collect_at(
                &target.schema,
                instance,
                depth + 1,
                target.resource_index,
                evaluated,
            )?;
            self.ref_hops -= 1;
            if violation.is_some() {
                return Ok(Some(Violation::Reference));
            }
            if self.dialect == Dialect::Draft7 {
                return Ok(None);
            }
        }
        if let Some(reference) = object.get("$dynamicRef") {
            self.consume_reference()?;
            let static_target = self
                .resolver
                .resolve(resource, string_value(reference).unwrap())?;
            let mut target_schema = static_target.schema.clone();
            let mut target_resource = static_target.resource_index;
            if static_target.anchor_kind == Some(AnchorKind::Dynamic) {
                let name = static_target.anchor_name.clone().unwrap();
                for scope_resource in &self.dynamic_scope {
                    if let Some(dynamic_target) = self.resolver.dynamic_anchor(*scope_resource, &name)
                    {
                        target_schema = dynamic_target;
                        target_resource = *scope_resource;
                        break;
                    }
                }
            }
            let violation = self.validate_collect_at(
                &target_schema,
                instance,
                depth + 1,
                target_resource,
                evaluated,
            )?;
            self.ref_hops -= 1;
            if violation.is_some() {
                return Ok(Some(Violation::Reference));
            }
        }

        if let Some(types) = object.get("type") {
            if !matches_type(instance, types, self.limits)? {
                return Ok(Some(Violation::Type));
            }
        }
        if let Some(constant) = object.get("const") {
            self.consume_step(depth + 1)?;
            if !json_equal(instance, constant, self.limits)? {
                return Ok(Some(Violation::Constant));
            }
        }
        if let Some(enumeration) = object.get("enum") {
            let mut matched = false;
            for candidate in enumeration.as_array().unwrap() {
                self.consume_step(depth + 1)?;
                if json_equal(instance, candidate, self.limits)? {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Ok(Some(Violation::Enumeration));
            }
        }

        if let Some(parsed_number) = instance_number(instance, self.limits)? {
            if let Some(value) = object.get("multipleOf") {
                let divisor = schema_number(value, self.limits)?;
                let is_multiple = parsed_number
                    .is_multiple_of(&divisor, number_limits(self.limits))
                    .map_err(|error| match error {
                        json_number::Error::OutOfMemory => Error::OutOfMemory,
                        json_number::Error::NumberLimitExceeded => Error::InstanceLimitExceeded,
                        json_number::Error::InvalidNumber => Error::InvalidSchema,
                    })?;
                if !is_multiple {
                    return Ok(Some(Violation::MultipleOf));
                }
            }
            if let Some(value) = object.get("maximum") {
                let bound = schema_number(value, self.limits)?;
                if parsed_number.order(&bound) == Ordering::Greater {
                    return Ok(Some(Violation::Maximum));
                }
            }
            if let Some(value) = object.get("exclusiveMaximum") {
                let bound = schema_number(value, self.limits)?;
                if parsed_number.order(&bound) != Ordering::Less {
                    return Ok(Some(Violation::ExclusiveMaximum));
                }
            }
            if let Some(value) = object.get("minimum") {
                let bound = schema_number(value, self.limits)?;
                if parsed_number.order(&bound) == Ordering::Less {
                    return Ok(Some(Violation::Minimum));
                }
            }
            if let Some(value) = object.get("exclusiveMinimum") {
                let bound = schema_number(value, self.limits)?;
                if parsed_number.order(&bound) != Ordering::Greater {
                    return Ok(Some(Violation::ExclusiveMinimum));
                }
            }
        }

        if let Value::String(text) = instance {
            let length = text.chars().count();
            if let Some(value) = object.get("minLength") {
                if length < non_negative_usize(value, self.limits)? {
                    return Ok(Some(Violation::MinLength));
                }
            }
            if let Some(value) = object.get("maxLength") {
                if length > non_negative_usize(value, self.limits)? {
                    return Ok(Some(Violation::MaxLength));
                }
            }
            if let Some(value) = object.get("pattern") {
                if !self.matches_pattern(string_value(value).unwrap().as_bytes(), text.as_bytes())? {
                    return Ok(Some(Violation::Pattern));
                }
            }
        }

        if let Value::Array(array) = instance {
            if let Some(violation) = self.validate_array(object, array, depth, evaluated)? {
                return Ok(Some(violation));
            }
        }
        if let Value::Object(map) = instance {
            if let Some(violation) = self.validate_object(object, map, depth, evaluated)? {
                return Ok(Some(violation));
            }
        }

        if let Some(all_of) = object.get("allOf") {
            for child in all_of.as_array().unwrap() {
                if self
                    .validate_collect(child, instance, depth + 1, evaluated)?
                    .is_some()
                {
                    return Ok(Some(Violation::AllOf));
                }
            }
        }
        if let Some(any_of) = object.get("anyOf") {
            let mut matched = false;
            for child in any_of.as_array().unwrap() {
                if self
                    .validate_collect(child, instance, depth + 1, evaluated)?
                    .is_none()
                {
                    matched = true;
                }
            }
            if !matched {
                return Ok(Some(Violation::AnyOf));
            }
        }
        if let Some(one_of) = object.get("oneOf") {
            let mut matches_count = 0usize;
            for child in one_of.as_array().unwrap() {
                if self
                    .validate_collect(child, instance, depth + 1, evaluated)?
                    .is_none()
                {
                    matches_count = matches_count
                        .checked_add(1)
                        .ok_or(Error::InstanceLimitExceeded)?;
                }
            }
            if matches_count != 1 {
                return Ok(Some(Violation::OneOf));
            }
        }
        if let Some(not_schema) = object.get("not") {
            if self.validate(not_schema, instance, depth + 1)?.is_none() {
                return Ok(Some(Violation::Not));
            }
        }
        if let Some(if_schema) = object.get("if") {
            let condition_matches = self
                .validate_collect(if_schema, instance, depth + 1, evaluated)?
                .is_none();
            let selected = if condition_matches {
                object.get("then")
            } else {
                object.get("else")
            };
            if let Some(branch) = selected {
                if self
                    .validate_collect(branch, instance, depth + 1, evaluated)?
                    .is_some()
                {
                    return Ok(Some(Violation::Conditional));
                }
            }
        }
        if self.dialect == Dialect::Draft202012 {
            if let Value::Array(array) = instance {
                if let Some(unevaluated) = object.get("unevaluatedItems") {
                    for (index, item) in array.iter().enumerate() {
                        if evaluated.item_is_marked(index) {
                            continue;
                        }
                        if self.validate(unevaluated, item, depth + 1)?.is_some() {
                            return Ok(Some(Violation::UnevaluatedItems));
                        }
                        evaluated.mark_item(index);
                    }
                }
            }
            if let Value::Object(map) = instance {
                if let Some(unevaluated) = object.get("unevaluatedProperties") {
                    for (index, (_, value)) in map.iter().enumerate() {
                        if evaluated.property_is_marked(index) {
                            continue;
                        }
                        if self.validate(unevaluated, value, depth + 1)?.is_some() {
                            return Ok(Some(Violation::UnevaluatedProperties));
                        }
                        evaluated.mark_property(index);
                    }
                }
            }
        }
        Ok(None)
    }

    fn validate_array(
        &mut self,
        schema: &JsonMap,
        instance: &[Value],
        depth: usize,
        evaluated: &mut Evaluation,
    ) -> StdResult<Option<Violation>> {
        if instance.len() > self.limits.max_container_entries {
            return Err(Error::InstanceLimitExceeded);
        }
        if let Some(value) = schema.get("minItems") {
            if instance.len() < non_negative_usize(value, self.limits)? {
                return Ok(Some(Violation::MinItems));
            }
        }
        if let Some(value) = schema.get("maxItems") {
            if instance.len() > non_negative_usize(value, self.limits)? {
                return Ok(Some(Violation::MaxItems));
            }
        }
        if let Some(value) = schema.get("uniqueItems") {
            if value.as_bool() == Some(true) {
                for left_index in 0..instance.len() {
                    for right in &instance[left_index + 1..] {
                        self.consume_step(depth + 1)?;
                        if json_equal(&instance[left_index], right, self.limits)? {
                            return Ok(Some(Violation::UniqueItems));
                        }
                    }
                }
            }
        }

        if self.dialect == Dialect::Draft7 {
            if let Some(items) = schema.get("items") {
                if let Value::Array(items_array) = items {
                    let tuple_count = items_array.len().min(instance.len());
                    for index in 0..tuple_count {
                        if self
                            .validate(&items_array[index], &instance[index], depth + 1)?
                            .is_some()
                        {
                            return Ok(Some(Violation::Type));
                        }
                    }
                    for index in 0..tuple_count {
                        evaluated.mark_item(index);
                    }
                    if let Some(additional) = schema.get("additionalItems") {
                        for index in tuple_count..instance.len() {
                            if self.validate(additional, &instance[index], depth + 1)?.is_some() {
                                return Ok(Some(Violation::AdditionalItems));
                            }
                            evaluated.mark_item(index);
                        }
                    }
                } else {
                    for index in 0..instance.len() {
                        if self.validate(items, &instance[index], depth + 1)?.is_some() {
                            return Ok(Some(Violation::Type));
                        }
                        evaluated.mark_item(index);
                    }
                }
            }
        } else {
            let mut prefix_count = 0usize;
            if let Some(prefix) = schema.get("prefixItems") {
                if let Value::Array(prefix_array) = prefix {
                    prefix_count = prefix_array.len().min(instance.len());
                    for index in 0..prefix_count {
                        if self
                            .validate(&prefix_array[index], &instance[index], depth + 1)?
                            .is_some()
                        {
                            return Ok(Some(Violation::Type));
                        }
                    }
                    for index in 0..prefix_count {
                        evaluated.mark_item(index);
                    }
                }
            }
            if let Some(items_schema) = schema.get("items") {
                for index in prefix_count..instance.len() {
                    if self
                        .validate(items_schema, &instance[index], depth + 1)?
                        .is_some()
                    {
                        return Ok(Some(Violation::Type));
                    }
                    evaluated.mark_item(index);
                }
            }
        }

        if let Some(contains_schema) = schema.get("contains") {
            let mut matches_count = 0usize;
            for index in 0..instance.len() {
                if self
                    .validate(contains_schema, &instance[index], depth + 1)?
                    .is_none()
                {
                    matches_count = matches_count
                        .checked_add(1)
                        .ok_or(Error::InstanceLimitExceeded)?;
                    evaluated.mark_item(index);
                }
            }
            let minimum = if self.dialect == Dialect::Draft202012 {
                if let Some(value) = schema.get("minContains") {
                    non_negative_usize(value, self.limits)?
                } else {
                    1
                }
            } else {
                1
            };
            let maximum = if self.dialect == Dialect::Draft202012 {
                if let Some(value) = schema.get("maxContains") {
                    non_negative_usize(value, self.limits)?
                } else {
                    usize::MAX
                }
            } else {
                usize::MAX
            };
            if matches_count < minimum || matches_count > maximum {
                return Ok(Some(Violation::Contains));
            }
        }
        Ok(None)
    }

    fn validate_object(
        &mut self,
        schema: &JsonMap,
        instance: &JsonMap,
        depth: usize,
        evaluated: &mut Evaluation,
    ) -> StdResult<Option<Violation>> {
        if instance.len() > self.limits.max_container_entries {
            return Err(Error::InstanceLimitExceeded);
        }
        if let Some(value) = schema.get("minProperties") {
            if instance.len() < non_negative_usize(value, self.limits)? {
                return Ok(Some(Violation::MinProperties));
            }
        }
        if let Some(value) = schema.get("maxProperties") {
            if instance.len() > non_negative_usize(value, self.limits)? {
                return Ok(Some(Violation::MaxProperties));
            }
        }
        if let Some(required) = schema.get("required") {
            for name in required.as_array().unwrap() {
                if !instance.contains(string_value(name).unwrap()) {
                    return Ok(Some(Violation::Required));
                }
            }
        }

        let properties = schema.get("properties");
        let pattern_properties = schema.get("patternProperties");
        let mut covered = vec![false; instance.len()];
        for (property_index, (key, value)) in instance.iter().enumerate() {
            if let Some(property_schemas) = properties {
                if let Some(property_schema) = property_schemas.as_object().unwrap().get(key) {
                    if self.validate(property_schema, value, depth + 1)?.is_some() {
                        return Ok(Some(Violation::Properties));
                    }
                    covered[property_index] = true;
                    evaluated.mark_property(property_index);
                }
            }
            if let Some(patterns) = pattern_properties {
                for (pattern, pattern_schema) in patterns.as_object().unwrap().iter() {
                    if !self.matches_pattern(pattern.as_bytes(), key.as_bytes())? {
                        continue;
                    }
                    if self.validate(pattern_schema, value, depth + 1)?.is_some() {
                        return Ok(Some(Violation::PatternProperties));
                    }
                    covered[property_index] = true;
                    evaluated.mark_property(property_index);
                }
            }
        }
        if let Some(additional) = schema.get("additionalProperties") {
            for (property_index, (_, value)) in instance.iter().enumerate() {
                if covered[property_index] {
                    continue;
                }
                if self.validate(additional, value, depth + 1)?.is_some() {
                    return Ok(Some(Violation::AdditionalProperties));
                }
                evaluated.mark_property(property_index);
            }
        }
        if let Some(property_names) = schema.get("propertyNames") {
            for (key, _) in instance.iter() {
                let key_value = Value::String(key.to_string());
                if self.validate(property_names, &key_value, depth + 1)?.is_some() {
                    return Ok(Some(Violation::PropertyNames));
                }
            }
        }
        if let Some(dependencies) = schema.get("dependentRequired") {
            for (key, names) in dependencies.as_object().unwrap().iter() {
                if !instance.contains(key) {
                    continue;
                }
                for name in names.as_array().unwrap() {
                    if !instance.contains(string_value(name).unwrap()) {
                        return Ok(Some(Violation::DependentRequired));
                    }
                }
            }
        }
        if let Some(dependencies) = schema.get("dependentSchemas") {
            for (key, dependency) in dependencies.as_object().unwrap().iter() {
                if !instance.contains(key) {
                    continue;
                }
                let object_value = Value::Object(instance.clone());
                if self
                    .validate_collect(dependency, &object_value, depth + 1, evaluated)?
                    .is_some()
                {
                    return Ok(Some(Violation::DependentSchemas));
                }
            }
        }
        if self.dialect == Dialect::Draft7 {
            if let Some(dependencies) = schema.get("dependencies") {
                for (key, dependency) in dependencies.as_object().unwrap().iter() {
                    if !instance.contains(key) {
                        continue;
                    }
                    if let Value::Array(names) = dependency {
                        for name in names {
                            if !instance.contains(string_value(name).unwrap()) {
                                return Ok(Some(Violation::Dependencies));
                            }
                        }
                    } else {
                        let object_value = Value::Object(instance.clone());
                        if self
                            .validate_collect(dependency, &object_value, depth + 1, evaluated)?
                            .is_some()
                        {
                            return Ok(Some(Violation::Dependencies));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn consume_step(&mut self, depth: usize) -> StdResult<()> {
        if depth > self.limits.max_depth {
            return Err(Error::InstanceLimitExceeded);
        }
        self.steps = self
            .steps
            .checked_add(1)
            .ok_or(Error::InstanceLimitExceeded)?;
        if self.steps > self.limits.max_steps {
            return Err(Error::InstanceLimitExceeded);
        }
        Ok(())
    }

    fn consume_reference(&mut self) -> StdResult<()> {
        self.ref_hops = self
            .ref_hops
            .checked_add(1)
            .ok_or(Error::InstanceLimitExceeded)?;
        if self.ref_hops > self.limits.max_ref_hops {
            return Err(Error::InstanceLimitExceeded);
        }
        Ok(())
    }

    fn matches_pattern(&mut self, source: &[u8], text: &[u8]) -> StdResult<bool> {
        json_schema_pattern::matches(
            source,
            text,
            pattern_limits(self.limits),
            &mut self.pattern_steps,
        )
        .map_err(|error| match error {
            json_schema_pattern::Error::OutOfMemory => Error::OutOfMemory,
            json_schema_pattern::Error::InvalidPattern
            | json_schema_pattern::Error::InvalidUtf8 => Error::InvalidSchema,
            json_schema_pattern::Error::PatternLimitExceeded => Error::InstanceLimitExceeded,
        })
    }
}

fn validate_type_keyword(value: &Value) -> StdResult<()> {
    if let Value::String(name) = value {
        return if valid_type_name(name) {
            Ok(())
        } else {
            Err(Error::InvalidSchema)
        };
    }
    let array = value.as_array().ok_or(Error::InvalidSchema)?;
    if array.is_empty() {
        return Err(Error::InvalidSchema);
    }
    for (index, item) in array.iter().enumerate() {
        let name = string_value(item).ok_or(Error::InvalidSchema)?;
        if !valid_type_name(name) {
            return Err(Error::InvalidSchema);
        }
        for previous in &array[..index] {
            if string_value(previous).unwrap() == name {
                return Err(Error::InvalidSchema);
            }
        }
    }
    Ok(())
}

fn validate_unique_string_array(value: &Value) -> StdResult<()> {
    let array = value.as_array().ok_or(Error::InvalidSchema)?;
    for (index, item) in array.iter().enumerate() {
        let name = string_value(item).ok_or(Error::InvalidSchema)?;
        for previous in &array[..index] {
            if string_value(previous).unwrap() == name {
                return Err(Error::InvalidSchema);
            }
        }
    }
    Ok(())
}

fn number_limits(limits: Limits) -> json_number::Limits {
    json_number::Limits {
        max_lexeme_bytes: limits.max_number_bytes,
        max_exponent_abs: limits.max_number_exponent_abs,
        max_expanded_digits: limits.max_number_expanded_digits,
    }
}

fn pattern_limits(limits: Limits) -> json_schema_pattern::Limits {
    json_schema_pattern::Limits {
        max_depth: limits.max_depth,
        max_states: limits.max_pattern_states,
        max_repeat: limits.max_pattern_repeat,
        max_steps: limits.max_pattern_steps,
    }
}

fn assess_pattern(source: &[u8], limits: Limits) -> StdResult<SchemaAssessment> {
    match json_schema_pattern::validate(source, pattern_limits(limits)) {
        Ok(()) => Ok(SchemaAssessment::LocallyEvaluable),
        Err(json_schema_pattern::Error::OutOfMemory) => Err(Error::OutOfMemory),
        Err(json_schema_pattern::Error::InvalidPattern) => Ok(SchemaAssessment::ServerAuthoritative),
        Err(json_schema_pattern::Error::InvalidUtf8) => Err(Error::InvalidSchema),
        Err(json_schema_pattern::Error::PatternLimitExceeded) => Err(Error::SchemaLimitExceeded),
    }
}

fn combine_assessment(left: SchemaAssessment, right: SchemaAssessment) -> SchemaAssessment {
    if left == SchemaAssessment::ServerAuthoritative || right == SchemaAssessment::ServerAuthoritative {
        SchemaAssessment::ServerAuthoritative
    } else {
        SchemaAssessment::LocallyEvaluable
    }
}

fn schema_number(value: &Value, limits: Limits) -> StdResult<Number> {
    match json_number::from_value(value, number_limits(limits)) {
        Ok(Some(number)) => Ok(number),
        Ok(None) => Err(Error::InvalidSchema),
        Err(json_number::Error::OutOfMemory) => Err(Error::OutOfMemory),
        Err(json_number::Error::NumberLimitExceeded) => Err(Error::SchemaLimitExceeded),
        Err(json_number::Error::InvalidNumber) => Err(Error::InvalidSchema),
    }
}

fn instance_number(value: &Value, limits: Limits) -> StdResult<Option<Number>> {
    match json_number::from_value(value, number_limits(limits)) {
        Ok(number) => Ok(number),
        Err(json_number::Error::OutOfMemory) => Err(Error::OutOfMemory),
        Err(json_number::Error::NumberLimitExceeded) => Err(Error::InstanceLimitExceeded),
        Err(json_number::Error::InvalidNumber) => Err(Error::InvalidJson),
    }
}

fn non_negative_usize(value: &Value, limits: Limits) -> StdResult<usize> {
    let number = schema_number(value, limits)?;
    number.to_non_negative_usize().ok_or(Error::InvalidSchema)
}

fn matches_type(instance: &Value, type_value: &Value, limits: Limits) -> StdResult<bool> {
    if let Value::String(name) = type_value {
        return matches_type_name(instance, name, limits);
    }
    for item in type_value.as_array().unwrap() {
        if matches_type_name(instance, string_value(item).unwrap(), limits)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn matches_type_name(instance: &Value, name: &str, limits: Limits) -> StdResult<bool> {
    if name == "null" {
        return Ok(instance.is_null());
    }
    if name == "boolean" {
        return Ok(instance.is_bool());
    }
    if name == "object" {
        return Ok(instance.is_object());
    }
    if name == "array" {
        return Ok(instance.is_array());
    }
    if name == "string" {
        return Ok(instance.is_string());
    }
    if name != "number" && name != "integer" {
        return Ok(false);
    }
    let parsed = instance_number(instance, limits)?;
    if parsed.is_none() {
        return Ok(false);
    }
    let number = parsed.unwrap();
    if name == "number" {
        return Ok(true);
    }
    Ok(number.is_integer())
}

fn string_value(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        _ => None,
    }
}

fn json_equal(left: &Value, right: &Value, limits: Limits) -> StdResult<bool> {
    if let Some(left_number) = instance_number(left, limits)? {
        let parsed_right = instance_number(right, limits)?;
        if parsed_right.is_none() {
            return Ok(false);
        }
        let right_number = parsed_right.unwrap();
        return Ok(left_number.eql(&right_number));
    }
    if instance_number(right, limits)?.is_some() {
        return Ok(false);
    }
    match (left, right) {
        (Value::Null, Value::Null) => Ok(true),
        (Value::Bool(left), Value::Bool(right)) => Ok(left == right),
        (Value::String(left), Value::String(right)) => Ok(left == right),
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Ok(false);
            }
            for (left_item, right_item) in left.iter().zip(right) {
                if !json_equal(left_item, right_item, limits)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Object(left), Value::Object(right)) => {
            if left.len() != right.len() {
                return Ok(false);
            }
            for (key, left_value) in left.iter() {
                let right_value = match right.get(key) {
                    Some(value) => value,
                    None => return Ok(false),
                };
                if !json_equal(left_value, right_value, limits)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn valid_type_name(name: &str) -> bool {
    name == "null"
        || name == "boolean"
        || name == "object"
        || name == "array"
        || name == "number"
        || name == "string"
        || name == "integer"
}

const SHARED_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "$ref",
    "$comment",
    "type",
    "enum",
    "const",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxProperties",
    "minProperties",
    "required",
    "items",
    "contains",
    "additionalProperties",
    "properties",
    "patternProperties",
    "propertyNames",
    "if",
    "then",
    "else",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "title",
    "description",
    "default",
    "readOnly",
    "writeOnly",
    "examples",
    "format",
    "contentEncoding",
    "contentMediaType",
];

const DRAFT_2020_EXTRA_KEYWORDS: &[&str] = &[
    "$vocabulary",
    "$dynamicRef",
    "$anchor",
    "$dynamicAnchor",
    "$defs",
    "maxContains",
    "minContains",
    "dependentRequired",
    "prefixItems",
    "dependentSchemas",
    "unevaluatedItems",
    "unevaluatedProperties",
    "deprecated",
    "contentSchema",
];

const DRAFT_7_EXTRA_KEYWORDS: &[&str] = &["definitions", "additionalItems", "dependencies"];

fn known_keyword(dialect: Dialect, keyword: &str) -> bool {
    if SHARED_KEYWORDS.contains(&keyword) {
        return true;
    }
    match dialect {
        Dialect::Draft202012 => DRAFT_2020_EXTRA_KEYWORDS.contains(&keyword),
        Dialect::Draft7 => DRAFT_7_EXTRA_KEYWORDS.contains(&keyword),
    }
}

fn schema_map_keyword(dialect: Dialect, keyword: &str) -> bool {
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
    schema_map_keyword(dialect, keyword)
        || (dialect == Dialect::Draft202012 && keyword == "prefixItems")
        || (dialect == Dialect::Draft202012 && keyword == "unevaluatedItems")
        || (dialect == Dialect::Draft202012 && keyword == "unevaluatedProperties")
        || (dialect == Dialect::Draft7 && keyword == "additionalItems")
        || keyword == "items"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_validation(schema: &str, instance: &str, expected_valid: bool) {
        let result = validate_text(schema, instance, Limits::default()).unwrap();
        assert_eq!(expected_valid, result == Result::Valid);
    }

    fn expect_schema_error(schema: &str, expected: Error) {
        assert!(matches!(
            validate_schema_text(schema, Limits::default()),
            Err(error) if error == expected
        ));
    }

    #[test]
    fn json_schema_rejects_invalid_array_keyword_shapes() {
        let invalid = [
            r##"{"required":["value","value"]}"##,
            r##"{"dependentRequired":{"value":["other","other"]}}"##,
            r##"{"allOf":[]}"##,
            r##"{"anyOf":[]}"##,
            r##"{"oneOf":[]}"##,
        ];
        for schema in invalid {
            expect_schema_error(schema, Error::InvalidSchema);
        }
        validate_schema_text(r##"{"prefixItems":[]}"##, Limits::default()).unwrap();
    }

    #[test]
    fn json_schema_rejects_rounded_programmatic_floats() {
        let schema = parse_json(r##"{"type":"number"}"##).unwrap();
        let instance = Value::Float(9_007_199_254_740_992.0);
        assert!(matches!(
            validate_value(&schema, &instance, Limits::default()),
            Err(Error::InvalidJson)
        ));
    }

    #[test]
    fn json_schema_2020_12_validates_core_applicator_and_validation_corpus_cases() {
        expect_validation(
            r##"{"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false}"##,
            r##"{"count":2}"##,
            true,
        );
        expect_validation(
            r##"{"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false}"##,
            r##"{"count":0}"##,
            false,
        );
        expect_validation(
            r##"{"oneOf":[{"type":"integer"},{"type":"number"}]}"##,
            "1",
            false,
        );
        expect_validation(
            r##"{"if":{"properties":{"kind":{"const":"a"}}},"then":{"required":["a"]},"else":{"required":["b"]}}"##,
            r##"{"kind":"a","a":1}"##,
            true,
        );
        expect_validation(
            r##"{"prefixItems":[{"type":"string"}],"items":{"type":"integer"},"contains":{"minimum":5},"minContains":1}"##,
            r##"["x",2,6]"##,
            true,
        );

        let cases: &[(&str, &str, bool)] = &[
            ("true", "null", true),
            ("false", "null", false),
            (r##"{"type":["string","null"]}"##, "null", true),
            (r#"{"enum":[1,"x",null]}"#, r#""y""#, false),
            (r#"{"const":{"a":[1,2]}}"#, r#"{"a":[1,2]}"#, true),
            (r##"{"multipleOf":0.25}"##, "1.5", true),
            (r##"{"multipleOf":0.25}"##, "1.6", false),
            (r##"{"minimum":2,"maximum":4}"##, "5", false),
            (r##"{"exclusiveMinimum":2,"exclusiveMaximum":4}"##, "2", false),
            (r#"{"minLength":2,"maxLength":2}"#, r#""éa""#, true),
            (r#"{"pattern":"^\\p{Letter}+$"}"#, r#""π""#, true),
            (r#"{"pattern":"^[a-z]+$"}"#, r#""123""#, false),
            (r##"{"minItems":2,"maxItems":3}"##, "[1]", false),
            (r#"{"uniqueItems":true}"#, r#"[{"a":1},{"a":1.0}]"#, false),
            (
                r##"{"contains":{"type":"integer"},"minContains":2,"maxContains":2}"##,
                r##"[1,2,"x"]"##,
                true,
            ),
            (
                r##"{"prefixItems":[{"type":"string"}],"items":false}"##,
                r##"["x",2]"##,
                false,
            ),
            (r#"{"minProperties":2,"maxProperties":2}"#, r#"{"a":1}"#, false),
            (r#"{"required":["a","b"]}"#, r#"{"a":1}"#, false),
            (r#"{"properties":{"a":{"type":"integer"}}}"#, r#"{"a":"x"}"#, false),
            (
                r##"{"properties":{"a":true},"additionalProperties":false}"##,
                r##"{"a":1,"b":2}"##,
                false,
            ),
            (
                r##"{"patternProperties":{"^x":{"type":"integer"}},"additionalProperties":false}"##,
                r##"{"x-value":1}"##,
                true,
            ),
            (r#"{"propertyNames":{"minLength":2}}"#, r#"{"a":1}"#, false),
            (
                r##"{"dependentRequired":{"card":["billing"]}}"##,
                r##"{"card":1}"##,
                false,
            ),
            (
                r##"{"dependentSchemas":{"card":{"required":["billing"]}}}"##,
                r##"{"card":1,"billing":2}"##,
                true,
            ),
            (r##"{"allOf":[{"type":"integer"},{"minimum":2}]}"##, "1", false),
            (
                r##"{"anyOf":[{"type":"integer"},{"type":"string"}]}"##,
                "true",
                false,
            ),
            (r#"{"not":{"type":"integer"}}"#, r#""x""#, true),
            (
                r##"{"allOf":[{"properties":{"known":true}}],"unevaluatedProperties":false}"##,
                r##"{"known":1}"##,
                true,
            ),
            (r##"{"prefixItems":[true],"unevaluatedItems":false}"##, "[1,2]", false),
            (
                r##"{"format":"email","contentEncoding":"base64"}"##,
                r##""not asserted""##,
                true,
            ),
        ];
        for (schema, instance, valid) in cases {
            expect_validation(schema, instance, *valid);
        }
    }

    #[test]
    fn json_schema_patterns_use_ecmascript_whitespace_semantics_in_both_dialects() {
        let non_space_schema = r##"{"pattern":"^\\S+$"}"##;
        let whitespace_instances = [
            r##""\u00A0""##,
            r##""\uFEFF""##,
            r##""\u2003""##,
            r##""\u2028""##,
            r##""\u2029""##,
        ];
        for instance in whitespace_instances {
            expect_validation(non_space_schema, instance, false);
        }
        expect_validation(non_space_schema, r##""plain""##, true);
        expect_validation(
            r##"{"patternProperties":{"^\\s$":{"type":"integer"}},"additionalProperties":false}"##,
            r##"{"\u2003":1}"##,
            true,
        );
        expect_validation(
            r##"{"patternProperties":{"^\\s$":{"type":"integer"}},"additionalProperties":false}"##,
            r##"{"\u2029":"invalid"}"##,
            false,
        );
        expect_validation(
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","pattern":"^\\S+$"}"##,
            r##""\u00A0""##,
            false,
        );
        expect_validation(
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","patternProperties":{"^\\s$":{"type":"integer"}},"additionalProperties":false}"##,
            r##"{"\uFEFF":1}"##,
            true,
        );
    }

    #[test]
    fn json_schema_compares_every_json_number_exactly() {
        expect_validation(r##"{"const":9007199254740992}"##, "9007199254740993", false);
        expect_validation(
            r##"{"enum":[9007199254740993]}"##,
            "9.007199254740993e15",
            true,
        );
        expect_validation(r##"{"minimum":9007199254740993}"##, "9007199254740992", false);
        expect_validation(r##"{"type":"integer"}"##, "9007199254740993.000", true);
        expect_validation(r##"{"type":"integer"}"##, "1e-1", false);
        expect_validation(r##"{"const":0}"##, "-0.0e999", true);
        expect_validation(r##"{"uniqueItems":true}"##, "[9007199254740992,9007199254740993]", true);
        expect_validation(r##"{"uniqueItems":true}"##, "[0,-0.0]", false);
        expect_validation(
            r##"{"multipleOf":0.000000000000000000000000000001}"##,
            "0.123456789012345678901234567890",
            true,
        );
        expect_validation(
            r##"{"multipleOf":0.000000000000000000000000000003}"##,
            "0.123456789012345678901234567890",
            true,
        );
        expect_validation(
            r##"{"multipleOf":0.000000000000000000000000000004}"##,
            "0.123456789012345678901234567890",
            false,
        );
        expect_validation(
            r##"{"const":0.12345678901234567890123456789}"##,
            "12345678901234567890123456789e-29",
            true,
        );
    }

    #[test]
    fn json_schema_resolves_local_pointer_anchor_and_dynamic_references_without_io() {
        expect_validation(
            r##"{"$defs":{"positive":{"$anchor":"positive","type":"integer","minimum":1}},"properties":{"a":{"$ref":"#/$defs/positive"},"b":{"$ref":"#positive"}}}"##,
            r##"{"a":2,"b":3}"##,
            true,
        );
        expect_validation(
            r##"{"$defs":{"node":{"$dynamicAnchor":"node","type":"object","properties":{"value":{"type":"integer"},"child":{"$dynamicRef":"#node"}}}},"$ref":"#/$defs/node"}"##,
            r##"{"value":1,"child":{"value":2}}"##,
            true,
        );
        expect_validation(
            r##"{"$id":"https://test.example/typical/root","$ref":"list","$defs":{"items":{"$dynamicAnchor":"items","type":"string"},"list":{"$id":"list","type":"array","items":{"$dynamicRef":"#items"},"$defs":{"bookend":{"$dynamicAnchor":"items"}}}}}"##,
            r##"["foo","bar"]"##,
            true,
        );
        expect_validation(
            r##"{"$id":"https://test.example/typical/root","$ref":"list","$defs":{"items":{"$dynamicAnchor":"items","type":"string"},"list":{"$id":"list","type":"array","items":{"$dynamicRef":"#items"},"$defs":{"bookend":{"$dynamicAnchor":"items"}}}}}"##,
            r##"["foo",42]"##,
            false,
        );
        expect_validation(
            r##"{"$id":"https://test.example/relative/root","$dynamicAnchor":"meta","properties":{"foo":{"const":"pass"}},"$ref":"extended","$defs":{"extended":{"$id":"extended","$dynamicAnchor":"meta","properties":{"bar":{"$ref":"bar"}}},"bar":{"$id":"bar","properties":{"baz":{"$dynamicRef":"extended#meta"}}}}}"##,
            r##"{"foo":"pass","bar":{"baz":{"foo":"pass"}}}"##,
            true,
        );
        expect_validation(
            r##"{"$id":"https://test.example/relative/root","$dynamicAnchor":"meta","properties":{"foo":{"const":"pass"}},"$ref":"extended","$defs":{"extended":{"$id":"extended","$dynamicAnchor":"meta","properties":{"bar":{"$ref":"bar"}}},"bar":{"$id":"bar","properties":{"baz":{"$dynamicRef":"extended#meta"}}}}}"##,
            r##"{"foo":"pass","bar":{"baz":{"foo":"fail"}}}"##,
            false,
        );
    }

    #[test]
    fn json_schema_selects_only_declared_supported_dialects() {
        let cases: &[(&str, Dialect)] = &[
            ("{}", Dialect::Draft202012),
            ("true", Dialect::Draft202012),
            (
                r##"{"$schema":"https://json-schema.org/draft/2020-12/schema"}"##,
                Dialect::Draft202012,
            ),
            (
                r##"{"$schema":"https://json-schema.org/draft/2020-12/schema#"}"##,
                Dialect::Draft202012,
            ),
            (
                r##"{"$schema":"http://json-schema.org/draft-07/schema"}"##,
                Dialect::Draft7,
            ),
            (
                r##"{"$schema":"http://json-schema.org/draft-07/schema#"}"##,
                Dialect::Draft7,
            ),
        ];
        for (schema, dialect) in cases {
            let parsed = parse_json(schema).unwrap();
            assert_eq!(*dialect, detect_dialect(&parsed).unwrap());
        }
        expect_schema_error(
            r##"{"$schema":"https://json-schema.org/draft/2019-09/schema"}"##,
            Error::UnsupportedDialect,
        );
        expect_schema_error(
            r##"{"$schema":"https://json-schema.org/draft-07/schema"}"##,
            Error::UnsupportedDialect,
        );
        expect_schema_error(r##"{"$schema":7}"##, Error::InvalidSchema);
    }

    #[test]
    fn json_schema_draft_7_validates_legacy_applicators_references_and_ref_siblings() {
        let prefix = r##"{"$schema":"http://json-schema.org/draft-07/schema#","##;
        expect_validation(
            &format!(
                r##"{}"definitions":{{"positive":{{"type":"integer","minimum":1}}}},"properties":{{"value":{{"$ref":"#/definitions/positive"}}}}}}"##,
                prefix
            ),
            r##"{"value":2}"##,
            true,
        );
        expect_validation(
            &format!(
                r##"{}"items":[{{"type":"integer"}},{{"type":"string"}}],"additionalItems":false}}"##,
                prefix
            ),
            r##"[1,"x",true]"##,
            false,
        );
        expect_validation(
            &format!(
                r##"{}"dependencies":{{"card":["billing"],"billing":{{"required":["address"]}}}}}}"##,
                prefix
            ),
            r##"{"card":1,"billing":2,"address":3}"##,
            true,
        );
        expect_validation(
            &format!(
                r##"{}"$id":"https://example.test/root","definitions":{{"number":{{"$id":"#number","type":"number"}}}},"properties":{{"value":{{"$ref":"#number"}}}}}}"##,
                prefix
            ),
            r##"{"value":1}"##,
            true,
        );
        expect_validation(
            &format!(
                r##"{}"definitions":{{"array":{{"type":"array"}}}},"$ref":"#/definitions/array","maxItems":1}}"##,
                prefix
            ),
            "[1,2]",
            true,
        );
    }

    #[test]
    fn json_schema_treats_other_dialect_keywords_as_annotations() {
        let schemas = [
            r##"{"definitions":{}}"##,
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","prefixItems":[]}"##,
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","deprecated":true}"##,
        ];
        for schema in schemas {
            assert_eq!(
                SchemaAssessment::LocallyEvaluable,
                validate_schema_text(schema, Limits::default()).unwrap()
            );
        }
    }

    #[test]
    fn json_schema_treats_unknown_keywords_as_annotations() {
        let schema = r##"{"type":"string","futureKeyword":{"type":7}}"##;
        let assessment = validate_schema_text(schema, Limits::default()).unwrap();
        assert!(assessment == SchemaAssessment::LocallyEvaluable);

        let result = validate_text(schema, "7", Limits::default()).unwrap();
        assert_eq!(Result::Invalid(Violation::Type), result);
    }

    #[test]
    fn json_schema_ignores_vocabulary_support_declarations_in_ordinary_schemas() {
        let schema = r##"{"$vocabulary":{"https://example.test/required":true},"type":"string"}"##;
        let assessment = validate_schema_text(schema, Limits::default()).unwrap();
        assert!(assessment == SchemaAssessment::LocallyEvaluable);

        let result = validate_text(schema, "7", Limits::default()).unwrap();
        assert_eq!(Result::Invalid(Violation::Type), result);

        expect_schema_error(r##"{"$vocabulary":[]}"##, Error::InvalidSchema);
        expect_schema_error(
            r##"{"$vocabulary":{"https://example.test/invalid":1}}"##,
            Error::InvalidSchema,
        );
    }

    #[test]
    fn json_schema_rejects_external_references_and_malformed_referenced_schemas() {
        expect_schema_error(r##"{"$ref":"https://example.com/schema"}"##, Error::ExternalReference);
        expect_schema_error(
            r##"{"title":"not a schema","$ref":"#/title"}"##,
            Error::InvalidSchema,
        );
    }

    #[test]
    fn json_schema_delegates_provider_patterns_outside_the_local_evaluator() {
        let schema = r##"{"type":"object","properties":{"contact":{"type":"object","properties":{"email":{"type":"string","pattern":"^(?!\.)(?!.*\.\.)[A-Za-z0-9_+.-]+@[A-Za-z0-9.-]+$"}}}}}"##;
        let schema_assessment = validate_schema_text(schema, Limits::default()).unwrap();
        assert!(schema_assessment == SchemaAssessment::ServerAuthoritative);

        let instance_assessment =
            validate_text(schema, r##"{"contact":{"email":"person@example.com"}}"##, Limits::default())
                .unwrap();
        assert!(instance_assessment == Result::ServerAuthoritative);
    }

    #[test]
    fn json_schema_delegates_pattern_properties_outside_the_local_evaluator() {
        let schema = r##"{"type":"object","patternProperties":{"^(?!secret$).*":{"type":"string"}}}"##;
        let schema_assessment = validate_schema_text(schema, Limits::default()).unwrap();
        assert!(schema_assessment == SchemaAssessment::ServerAuthoritative);

        let instance_assessment =
            validate_text(schema, r##"{"public":"value"}"##, Limits::default()).unwrap();
        assert!(instance_assessment == Result::ServerAuthoritative);
    }

    #[test]
    fn json_schema_hard_errors_dominate_delegated_patterns_regardless_of_order() {
        let malformed_siblings = [
            r##"{"pattern":"(?=a)","minLength":"invalid"}"##,
            r##"{"minLength":"invalid","pattern":"(?=a)"}"##,
            r##"{"patternProperties":{"(?=a)":true},"required":7}"##,
            r##"{"required":7,"patternProperties":{"(?=a)":true}}"##,
            r##"{"properties":{"delegated":{"pattern":"(?=a)"},"malformed":{"required":7}}}"##,
            r##"{"properties":{"malformed":{"required":7},"delegated":{"pattern":"(?=a)"}}}"##,
        ];
        for schema in malformed_siblings {
            expect_schema_error(schema, Error::InvalidSchema);
            assert!(matches!(
                validate_text(schema, "{}", Limits::default()),
                Err(Error::InvalidSchema)
            ));
        }

        let external_references = [
            r##"{"pattern":"(?=a)","$ref":"https://example.test/schema"}"##,
            r##"{"$ref":"https://example.test/schema","pattern":"(?=a)"}"##,
        ];
        for schema in external_references {
            expect_schema_error(schema, Error::ExternalReference);
            assert!(matches!(
                validate_text(schema, "{}", Limits::default()),
                Err(Error::ExternalReference)
            ));
        }

        let referenced_malformed_schema =
            r##"{"pattern":"(?=a)","$ref":"#/default","default":{"type":7}}"##;
        expect_schema_error(referenced_malformed_schema, Error::InvalidSchema);
        assert!(matches!(
            validate_text(referenced_malformed_schema, "{}", Limits::default()),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn json_schema_rejects_malformed_schemas_reached_through_annotation_references() {
        let schemas = [
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","default":{"type":7},"$ref":"#/default"}"##,
            r##"{"default":{"type":7},"$ref":"#/default"}"##,
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","default":{"$ref":"#/examples/0"},"examples":[{"type":7}],"$ref":"#/default"}"##,
            r##"{"default":{"$ref":"#/examples/0"},"examples":[{"type":7}],"$ref":"#/default"}"##,
        ];
        for schema in schemas {
            expect_schema_error(schema, Error::InvalidSchema);
            assert!(matches!(
                validate_text(schema, "{}", Limits::default()),
                Err(Error::InvalidSchema)
            ));
        }
    }

    #[test]
    fn json_schema_publication_preserves_recursive_annotation_references() {
        validate_schema_text(
            r##"{"default":{"$ref":"#/default"},"$ref":"#/default"}"##,
            Limits::default(),
        )
        .unwrap();
        validate_schema_text(
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","default":{"$ref":"#/default"},"$ref":"#/default"}"##,
            Limits::default(),
        )
        .unwrap();
    }

    #[test]
    fn json_schema_delegates_pattern_grammar_the_local_evaluator_cannot_classify() {
        let delegated = [
            r##"{"pattern":"[\\s-a]"}"##,
            r##"{"pattern":"[\\w-a]"}"##,
            r##"{"pattern":"(a)\\1"}"##,
            r##"{"pattern":"^*"}"##,
            r##"{"pattern":"$+"}"##,
        ];
        for schema in delegated {
            assert_eq!(
                SchemaAssessment::ServerAuthoritative,
                validate_schema_text(schema, Limits::default()).unwrap()
            );
        }
        assert_eq!(
            SchemaAssessment::LocallyEvaluable,
            validate_schema_text(r##"{"pattern":"[\\s-]"}"##, Limits::default()).unwrap()
        );
        assert_eq!(
            SchemaAssessment::LocallyEvaluable,
            validate_schema_text(r##"{"pattern":"(^)?"}"##, Limits::default()).unwrap()
        );
    }

    #[test]
    fn json_schema_keeps_invalid_utf8_pattern_inputs_hard() {
        let invalid_utf8 = [0xffu8];
        assert!(matches!(
            assess_pattern(&invalid_utf8, Limits::default()),
            Err(Error::InvalidSchema)
        ));

        let parsed = parse_json("true").unwrap();
        let resolver =
            Resolver::init(parsed, Dialect::Draft202012, resolver_limits(Limits::default()))
                .unwrap();
        let mut ctx = ValidationContext {
            dialect: Dialect::Draft202012,
            resolver: &resolver,
            limits: Limits::default(),
            steps: 0,
            pattern_steps: 0,
            ref_hops: 0,
            dynamic_scope: Vec::new(),
        };
        assert!(matches!(
            ctx.matches_pattern(b"a", &invalid_utf8),
            Err(Error::InvalidSchema)
        ));
    }

    #[test]
    fn json_schema_rejects_identifiers_reached_through_annotation_pointers_without_aborting() {
        let schemas = [
            r##"{"$ref":"#/default","default":{"$id":"child","type":"string"}}"##,
            r##"{"$dynamicRef":"#/const","const":{"$id":"child","type":"string"}}"##,
            r##"{"$ref":"#/examples/0","examples":[{"$id":"child","type":"string"}]}"##,
            r##"{"$dynamicRef":"#/enum/0","enum":[{"$id":"child","type":"string"}]}"##,
        ];
        for schema in schemas {
            assert!(matches!(
                validate_text(schema, r##""value""##, Limits::default()),
                Err(Error::InvalidSchema)
            ));
        }
    }

    #[test]
    fn json_schema_bounds_recursive_references_and_container_work() {
        assert!(matches!(
            validate_text(
                r##"{"$defs":{"loop":{"$ref":"#/$defs/loop"}},"$ref":"#/$defs/loop"}"##,
                "null",
                Limits {
                    max_ref_hops: 4,
                    ..Limits::default()
                },
            ),
            Err(Error::InstanceLimitExceeded)
        ));
        assert!(matches!(
            validate_text(
                r##"{"$schema":"http://json-schema.org/draft-07/schema#","definitions":{"loop":{"$ref":"#/definitions/loop"}},"$ref":"#/definitions/loop"}"##,
                "null",
                Limits {
                    max_ref_hops: 4,
                    ..Limits::default()
                },
            ),
            Err(Error::InstanceLimitExceeded)
        ));
        assert!(matches!(
            validate_schema_text(
                r##"{"$defs":{"a":true,"b":true}}"##,
                Limits {
                    max_nodes: 2,
                    ..Limits::default()
                },
            ),
            Err(Error::SchemaLimitExceeded)
        ));
        assert!(matches!(
            validate_schema_text(r##"{"pattern":"a{1025}"}"##, Limits::default()),
            Err(Error::SchemaLimitExceeded)
        ));
        assert!(matches!(
            validate_text(r#"{"pattern":"a{1025}"}"#, r#""a""#, Limits::default()),
            Err(Error::SchemaLimitExceeded)
        ));

        let runtime_pattern = r##"{"pattern":"(a|aa)*b"}"##;
        assert_eq!(
            SchemaAssessment::LocallyEvaluable,
            validate_schema_text(
                runtime_pattern,
                Limits {
                    max_pattern_steps: 64,
                    ..Limits::default()
                },
            )
            .unwrap()
        );
        assert!(matches!(
            validate_text(
                runtime_pattern,
                r##""aaaaaaaaaaaaaaaa""##,
                Limits {
                    max_pattern_steps: 64,
                    ..Limits::default()
                },
            ),
            Err(Error::InstanceLimitExceeded)
        ));
        assert!(matches!(
            validate_text(
                "true",
                "[[[0]]]",
                Limits {
                    max_depth: 2,
                    ..Limits::default()
                },
            ),
            Err(Error::InstanceLimitExceeded)
        ));
        validate_schema_text(
            r##"{"pattern":"a{2}"}"##,
            Limits {
                max_pattern_repeat: 2,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(matches!(
            validate_schema_text(
                r##"{"pattern":"a{3}"}"##,
                Limits {
                    max_pattern_repeat: 2,
                    ..Limits::default()
                },
            ),
            Err(Error::SchemaLimitExceeded)
        ));
        validate_schema_text(
            r##"{"required":["a","b"]}"##,
            Limits {
                max_container_entries: 2,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(matches!(
            validate_schema_text(
                r##"{"required":["a","b","c"]}"##,
                Limits {
                    max_container_entries: 2,
                    ..Limits::default()
                },
            ),
            Err(Error::SchemaLimitExceeded)
        ));
    }

    #[test]
    fn json_schema_parser_and_evaluator_stay_bounded() {
        let limits = Limits {
            max_schema_bytes: 1024,
            max_instance_bytes: 1024,
            max_depth: 16,
            max_nodes: 256,
            max_steps: 2048,
            max_ref_hops: 32,
            max_container_entries: 128,
            ..Limits::default()
        };
        let corpus = ["{}{}", "true[]", r#"{"type":"object"}{"a":1}"#, r##"{"$ref":"#/$defs/x","$defs":{"x":true}}null"##];
        for entry in corpus {
            let middle = entry.len() / 2;
            let _ = validate_text(&entry[..middle], &entry[middle..], limits);
        }
    }

    #[test]
    fn json_schema_validation_releases_every_allocation_failure_path() {
        let result = validate_text(
            r##"{"type":"object","patternProperties":{"^v":{"$ref":"#/$defs/positive"}},"unevaluatedProperties":false,"$defs":{"positive":{"type":"integer","minimum":1}},"required":["value"]}"##,
            r##"{"value":2}"##,
            Limits::default(),
        )
        .unwrap();
        assert_eq!(Result::Valid, result);

        let draft7 = validate_text(
            r##"{"$schema":"http://json-schema.org/draft-07/schema#","definitions":{"positive":{"type":"integer","minimum":1}},"items":[{"$ref":"#/definitions/positive"}],"additionalItems":false}"##,
            "[2]",
            Limits::default(),
        )
        .unwrap();
        assert_eq!(Result::Valid, draft7);
    }
}
