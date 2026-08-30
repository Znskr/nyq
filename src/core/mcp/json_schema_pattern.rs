use super::json_schema_unicode_letter as unicode_letter;

const INVALID_INDEX: usize = usize::MAX;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_depth: usize,
    pub max_states: usize,
    pub max_repeat: usize,
    pub max_steps: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidPattern,
    PatternLimitExceeded,
    InvalidUtf8,
    OutOfMemory,
}

#[derive(Clone, Copy)]
struct Pair {
    left: usize,
    right: usize,
}

#[derive(Clone, Copy)]
struct Repeat {
    child: usize,
    min: usize,
    max: Option<usize>,
}

#[derive(Clone, Copy)]
struct Atom {
    node: usize,
    quantifiable: bool,
}

#[derive(Clone, Copy)]
enum Node {
    Empty,
    Literal(char),
    Any,
    Class(usize),
    Concat(Pair),
    Alternate(Pair),
    Repeat(Repeat),
    AssertStart,
    AssertEnd,
}

#[derive(Clone, Copy)]
struct Range {
    first: char,
    last: char,
}

fn is_ecmascript_whitespace(codepoint: char) -> bool {
    matches!(
        codepoint,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

#[derive(Clone, Copy)]
struct CharacterClass {
    range_start: usize,
    range_len: usize,
    negated: bool,
    ascii_digit: bool,
    ascii_word: bool,
    ecmascript_space: bool,
    not_ecmascript_space: bool,
    unicode_letter: bool,
}

impl CharacterClass {
    fn empty(range_start: usize) -> Self {
        Self {
            range_start,
            range_len: 0,
            negated: false,
            ascii_digit: false,
            ascii_word: false,
            ecmascript_space: false,
            not_ecmascript_space: false,
            unicode_letter: false,
        }
    }

    fn matches(&self, ranges: &[Range], codepoint: char) -> bool {
        let mut matched = self.unicode_letter && unicode_letter::contains(codepoint);
        matched = matched || (self.ascii_digit && codepoint >= '0' && codepoint <= '9');
        matched = matched
            || (self.ascii_word
                && ((codepoint >= 'a' && codepoint <= 'z')
                    || (codepoint >= 'A' && codepoint <= 'Z')
                    || (codepoint >= '0' && codepoint <= '9')
                    || codepoint == '_'));
        let is_space = is_ecmascript_whitespace(codepoint);
        matched = matched || (self.ecmascript_space && is_space);
        matched = matched || (self.not_ecmascript_space && !is_space);
        for range in &ranges[self.range_start..self.range_start + self.range_len] {
            if codepoint >= range.first && codepoint <= range.last {
                matched = true;
                break;
            }
        }
        if self.negated {
            !matched
        } else {
            matched
        }
    }
}

struct Parser<'a> {
    source: &'a [u8],
    limits: Limits,
    index: usize,
    group_depth: usize,
    nodes: Vec<Node>,
    classes: Vec<CharacterClass>,
    ranges: Vec<Range>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a [u8], limits: Limits) -> Self {
        Self {
            source,
            limits,
            index: 0,
            group_depth: 0,
            nodes: Vec::new(),
            classes: Vec::new(),
            ranges: Vec::new(),
        }
    }

    fn parse(&mut self) -> Result<usize, Error> {
        let root = self.parse_alternation(None)?;
        if self.index != self.source.len() {
            return Err(Error::InvalidPattern);
        }
        Ok(root)
    }

    fn parse_alternation(&mut self, terminator: Option<u8>) -> Result<usize, Error> {
        let mut left = self.parse_sequence(terminator)?;
        while self.peek_byte() == Some(b'|') {
            self.index += 1;
            let right = self.parse_sequence(terminator)?;
            left = self.add_node(Node::Alternate(Pair { left, right }))?;
        }
        Ok(left)
    }

    fn parse_sequence(&mut self, terminator: Option<u8>) -> Result<usize, Error> {
        let mut result: Option<usize> = None;
        while self.index < self.source.len() {
            let byte = self.source[self.index];
            if byte == b'|' || terminator == Some(byte) {
                break;
            }
            let item = self.parse_repetition()?;
            result = match result {
                Some(left) => Some(self.add_node(Node::Concat(Pair { left, right: item }))?),
                None => Some(item),
            };
        }
        match result {
            Some(node) => Ok(node),
            None => self.add_node(Node::Empty),
        }
    }

    fn parse_repetition(&mut self) -> Result<usize, Error> {
        let atom = self.parse_atom()?;
        if self.index >= self.source.len() {
            return Ok(atom.node);
        }
        if !atom.quantifiable {
            match self.source[self.index] {
                b'*' | b'+' | b'?' | b'{' => return Err(Error::InvalidPattern),
                _ => {}
            }
        }
        let min: usize;
        let max: Option<usize>;
        match self.source[self.index] {
            b'*' => {
                self.index += 1;
                min = 0;
                max = None;
            }
            b'+' => {
                self.index += 1;
                min = 1;
                max = None;
            }
            b'?' => {
                self.index += 1;
                min = 0;
                max = Some(1);
            }
            b'{' => {
                self.index += 1;
                min = self.parse_count()?;
                if self.consume_byte(b'}') {
                    max = Some(min);
                } else {
                    if !self.consume_byte(b',') {
                        return Err(Error::InvalidPattern);
                    }
                    max = if self.consume_byte(b'}') {
                        None
                    } else {
                        let upper = self.parse_count()?;
                        if !self.consume_byte(b'}') {
                            return Err(Error::InvalidPattern);
                        }
                        Some(upper)
                    };
                }
            }
            _ => return Ok(atom.node),
        }
        if let Some(upper) = max {
            if upper < min {
                return Err(Error::InvalidPattern);
            }
        }
        if min > self.limits.max_repeat || max.unwrap_or(min) > self.limits.max_repeat {
            return Err(Error::PatternLimitExceeded);
        }
        let _ = self.consume_byte(b'?'); // Lazy quantifiers have identical acceptance semantics.
        if self.index < self.source.len() {
            match self.source[self.index] {
                b'*' | b'+' | b'?' | b'{' => return Err(Error::InvalidPattern),
                _ => {}
            }
        }
        self.add_node(Node::Repeat(Repeat {
            child: atom.node,
            min,
            max,
        }))
    }

    fn parse_atom(&mut self) -> Result<Atom, Error> {
        if self.index >= self.source.len() {
            return Err(Error::InvalidPattern);
        }
        let byte = self.source[self.index];
        match byte {
            b'^' => {
                self.index += 1;
                Ok(Atom {
                    node: self.add_node(Node::AssertStart)?,
                    quantifiable: false,
                })
            }
            b'$' => {
                self.index += 1;
                Ok(Atom {
                    node: self.add_node(Node::AssertEnd)?,
                    quantifiable: false,
                })
            }
            b'.' => {
                self.index += 1;
                Ok(Atom {
                    node: self.add_node(Node::Any)?,
                    quantifiable: true,
                })
            }
            b'(' => {
                self.index += 1;
                if self.peek_byte() == Some(b'?') {
                    return Err(Error::InvalidPattern);
                }
                if self.group_depth >= self.limits.max_depth {
                    return Err(Error::PatternLimitExceeded);
                }
                self.group_depth += 1;
                let nested = self.parse_alternation(Some(b')'))?;
                self.group_depth -= 1;
                if !self.consume_byte(b')') {
                    return Err(Error::InvalidPattern);
                }
                Ok(Atom {
                    node: nested,
                    quantifiable: true,
                })
            }
            b'[' => Ok(Atom {
                node: self.parse_character_class()?,
                quantifiable: true,
            }),
            b'\\' => Ok(Atom {
                node: self.parse_escape_node()?,
                quantifiable: true,
            }),
            b')' | b']' | b'}' | b'|' | b'*' | b'+' | b'?' | b'{' => Err(Error::InvalidPattern),
            _ => {
                let codepoint = self.next_codepoint()?;
                Ok(Atom {
                    node: self.add_node(Node::Literal(codepoint))?,
                    quantifiable: true,
                })
            }
        }
    }

    fn parse_escape_node(&mut self) -> Result<usize, Error> {
        self.index += 1;
        if self.index >= self.source.len() {
            return Err(Error::InvalidPattern);
        }
        let byte = self.source[self.index];
        self.index += 1;
        match byte {
            b'd' | b'D' | b'w' | b'W' | b's' | b'S' => {
                let mut class = CharacterClass::empty(self.ranges.len());
                class.negated = byte.is_ascii_uppercase();
                match byte.to_ascii_lowercase() {
                    b'd' => class.ascii_digit = true,
                    b'w' => class.ascii_word = true,
                    b's' => class.ecmascript_space = true,
                    _ => unreachable!(),
                }
                self.add_class_node(class)
            }
            b'p' | b'P' => {
                if !self.consume_byte(b'{') {
                    return Err(Error::InvalidPattern);
                }
                let start = self.index;
                while self.index < self.source.len() && self.source[self.index] != b'}' {
                    self.index += 1;
                }
                if !self.consume_byte(b'}') {
                    return Err(Error::InvalidPattern);
                }
                let property = &self.source[start..self.index - 1];
                if property != b"Letter" && property != b"L" {
                    return Err(Error::InvalidPattern);
                }
                let mut class = CharacterClass::empty(self.ranges.len());
                class.negated = byte == b'P';
                class.unicode_letter = true;
                self.add_class_node(class)
            }
            b'n' => self.add_node(Node::Literal('\n')),
            b'r' => self.add_node(Node::Literal('\r')),
            b't' => self.add_node(Node::Literal('\t')),
            b'f' => self.add_node(Node::Literal('\u{000C}')),
            b'v' => self.add_node(Node::Literal('\u{000B}')),
            b'x' => {
                let codepoint = self.parse_hex_codepoint(2)?;
                self.add_node(Node::Literal(codepoint))
            }
            b'u' => {
                if self.consume_byte(b'{') {
                    let codepoint = self.parse_variable_hex_codepoint()?;
                    if !self.consume_byte(b'}') {
                        return Err(Error::InvalidPattern);
                    }
                    self.add_node(Node::Literal(codepoint))
                } else {
                    let codepoint = self.parse_hex_codepoint(4)?;
                    self.add_node(Node::Literal(codepoint))
                }
            }
            b'0' => {
                if let Some(next) = self.peek_byte() {
                    if next.is_ascii_digit() {
                        return Err(Error::InvalidPattern);
                    }
                }
                self.add_node(Node::Literal('\0'))
            }
            b'1'..=b'9' | b'b' | b'B' | b'c' | b'k' => Err(Error::InvalidPattern),
            _ => {
                if !is_identity_escape(byte, false) {
                    return Err(Error::InvalidPattern);
                }
                self.add_node(Node::Literal(byte as char))
            }
        }
    }

    fn parse_character_class(&mut self) -> Result<usize, Error> {
        self.index += 1;
        let mut class = CharacterClass::empty(self.ranges.len());
        class.negated = self.consume_byte(b'^');
        let mut first = true;
        while self.index < self.source.len() {
            if self.source[self.index] == b']' && !first {
                self.index += 1;
                class.range_len = self.ranges.len() - class.range_start;
                return self.add_class_node(class);
            }
            let left = self.parse_class_atom(&mut class)?;
            first = false;
            if left.is_none() {
                if self.peek_byte() == Some(b'-')
                    && self.index + 1 < self.source.len()
                    && self.source[self.index + 1] != b']'
                {
                    return Err(Error::InvalidPattern);
                }
                continue;
            }
            let left = left.unwrap();
            if self.peek_byte() == Some(b'-')
                && self.index + 1 < self.source.len()
                && self.source[self.index + 1] != b']'
            {
                self.index += 1;
                let right = self
                    .parse_class_atom(&mut class)?
                    .ok_or(Error::InvalidPattern)?;
                if right < left {
                    return Err(Error::InvalidPattern);
                }
                self.ranges.push(Range {
                    first: left,
                    last: right,
                });
            } else {
                self.ranges.push(Range {
                    first: left,
                    last: left,
                });
            }
            if self.ranges.len() - class.range_start > self.limits.max_states {
                return Err(Error::PatternLimitExceeded);
            }
        }
        Err(Error::InvalidPattern)
    }

    fn parse_class_atom(&mut self, class: &mut CharacterClass) -> Result<Option<char>, Error> {
        if self.index >= self.source.len() {
            return Err(Error::InvalidPattern);
        }
        if self.source[self.index] != b'\\' {
            return self.next_codepoint().map(Some);
        }
        self.index += 1;
        if self.index >= self.source.len() {
            return Err(Error::InvalidPattern);
        }
        let byte = self.source[self.index];
        self.index += 1;
        match byte {
            b'd' => {
                class.ascii_digit = true;
                Ok(None)
            }
            b'w' => {
                class.ascii_word = true;
                Ok(None)
            }
            b's' => {
                class.ecmascript_space = true;
                Ok(None)
            }
            b'S' => {
                class.not_ecmascript_space = true;
                Ok(None)
            }
            b'D' | b'W' | b'p' | b'P' | b'b' | b'B' | b'1'..=b'9' | b'c' | b'k' => {
                Err(Error::InvalidPattern)
            }
            b'0' => {
                if let Some(next) = self.peek_byte() {
                    if next.is_ascii_digit() {
                        return Err(Error::InvalidPattern);
                    }
                }
                Ok(Some('\0'))
            }
            b'n' => Ok(Some('\n')),
            b'r' => Ok(Some('\r')),
            b't' => Ok(Some('\t')),
            b'f' => Ok(Some('\u{000C}')),
            b'v' => Ok(Some('\u{000B}')),
            b'x' => self.parse_hex_codepoint(2).map(Some),
            b'u' => self.parse_hex_codepoint(4).map(Some),
            _ => {
                if !is_identity_escape(byte, true) {
                    return Err(Error::InvalidPattern);
                }
                Ok(Some(byte as char))
            }
        }
    }

    fn add_class_node(&mut self, class: CharacterClass) -> Result<usize, Error> {
        if self.classes.len() >= self.limits.max_states {
            return Err(Error::PatternLimitExceeded);
        }
        let index = self.classes.len();
        self.classes.push(class);
        self.add_node(Node::Class(index))
    }

    fn add_node(&mut self, node: Node) -> Result<usize, Error> {
        if self.nodes.len() >= self.limits.max_states {
            return Err(Error::PatternLimitExceeded);
        }
        let index = self.nodes.len();
        self.nodes.push(node);
        Ok(index)
    }

    fn parse_count(&mut self) -> Result<usize, Error> {
        let start = self.index;
        while self.index < self.source.len() && self.source[self.index].is_ascii_digit() {
            self.index += 1;
        }
        if self.index == start {
            return Err(Error::InvalidPattern);
        }
        let text = std::str::from_utf8(&self.source[start..self.index])
            .map_err(|_| Error::InvalidPattern)?;
        text.parse::<usize>().map_err(|_| Error::PatternLimitExceeded)
    }

    fn parse_hex_codepoint(&mut self, digits: usize) -> Result<char, Error> {
        if digits > self.source.len() - self.index {
            return Err(Error::InvalidPattern);
        }
        let end = self.index + digits;
        let text = std::str::from_utf8(&self.source[self.index..end])
            .map_err(|_| Error::InvalidPattern)?;
        let value = u32::from_str_radix(text, 16).map_err(|_| Error::InvalidPattern)?;
        self.index = end;
        char::from_u32(value).ok_or(Error::InvalidPattern)
    }

    fn parse_variable_hex_codepoint(&mut self) -> Result<char, Error> {
        let start = self.index;
        while self.index < self.source.len() && self.source[self.index] != b'}' {
            self.index += 1;
        }
        if self.index == start || self.index - start > 6 {
            return Err(Error::InvalidPattern);
        }
        let text = std::str::from_utf8(&self.source[start..self.index])
            .map_err(|_| Error::InvalidPattern)?;
        let value = u32::from_str_radix(text, 16).map_err(|_| Error::InvalidPattern)?;
        char::from_u32(value).ok_or(Error::InvalidPattern)
    }

    fn next_codepoint(&mut self) -> Result<char, Error> {
        let (codepoint, width) =
            decode_utf8(&self.source[self.index..]).map_err(|_| Error::InvalidUtf8)?;
        self.index += width;
        Ok(codepoint)
    }

    fn peek_byte(&self) -> Option<u8> {
        self.source.get(self.index).copied()
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        if self.peek_byte() != Some(expected) {
            return false;
        }
        self.index += 1;
        true
    }
}

fn is_identity_escape(byte: u8, in_class: bool) -> bool {
    match byte {
        b'^' | b'$' | b'\\' | b'.' | b'*' | b'+' | b'?' | b'(' | b')' | b'[' | b']' | b'{'
        | b'}' | b'|' | b'/' => true,
        b'-' => in_class,
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum Op {
    Epsilon,
    Literal,
    Any,
    Class,
    Split,
    AssertStart,
    AssertEnd,
    Accept,
}

#[derive(Clone, Copy)]
struct Instruction {
    op: Op,
    out: usize,
    out1: usize,
    value: usize,
}

impl Instruction {
    fn new(op: Op) -> Self {
        Self {
            op,
            out: INVALID_INDEX,
            out1: INVALID_INDEX,
            value: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct Patch {
    instruction: usize,
    second: bool,
}

struct Fragment {
    start: usize,
    outs: Vec<Patch>,
}

struct Compiler<'a> {
    nodes: &'a [Node],
    limits: Limits,
    instructions: Vec<Instruction>,
}

impl<'a> Compiler<'a> {
    fn new(nodes: &'a [Node], limits: Limits) -> Self {
        Self {
            nodes,
            limits,
            instructions: Vec::new(),
        }
    }

    fn compile(&mut self, node_index: usize) -> Result<usize, Error> {
        let fragment = self.compile_node(node_index)?;
        let accept = self.add_instruction(Instruction::new(Op::Accept))?;
        self.patch(&fragment.outs, accept);
        Ok(fragment.start)
    }

    fn compile_node(&mut self, node_index: usize) -> Result<Fragment, Error> {
        match &self.nodes[node_index] {
            Node::Empty => self.single(Instruction::new(Op::Epsilon), false),
            Node::Literal(value) => {
                let mut instruction = Instruction::new(Op::Literal);
                instruction.value = *value as usize;
                self.single(instruction, false)
            }
            Node::Any => self.single(Instruction::new(Op::Any), false),
            Node::Class(value) => {
                let mut instruction = Instruction::new(Op::Class);
                instruction.value = *value;
                self.single(instruction, false)
            }
            Node::AssertStart => self.single(Instruction::new(Op::AssertStart), false),
            Node::AssertEnd => self.single(Instruction::new(Op::AssertEnd), false),
            Node::Concat(pair) => self.compile_concat(*pair),
            Node::Alternate(pair) => self.compile_alternate(*pair),
            Node::Repeat(repeat) => self.compile_repeat(*repeat),
        }
    }

    fn compile_concat(&mut self, pair: Pair) -> Result<Fragment, Error> {
        let left = self.compile_node(pair.left)?;
        let right = self.compile_node(pair.right)?;
        self.concat(left, right)
    }

    fn compile_alternate(&mut self, pair: Pair) -> Result<Fragment, Error> {
        let left = self.compile_node(pair.left)?;
        let right = self.compile_node(pair.right)?;
        self.alternate(left, right)
    }

    fn compile_repeat(&mut self, repeat: Repeat) -> Result<Fragment, Error> {
        let mut result = self.single(Instruction::new(Op::Epsilon), false)?;
        let mut count = 0usize;
        while count < repeat.min {
            let child = self.compile_node(repeat.child)?;
            result = self.concat(result, child)?;
            count += 1;
        }
        if let Some(maximum) = repeat.max {
            while count < maximum {
                let child = self.compile_node(repeat.child)?;
                let optional = self.optional(child)?;
                result = self.concat(result, optional)?;
                count += 1;
            }
        } else {
            let child = self.compile_node(repeat.child)?;
            let star = self.star(child)?;
            result = self.concat(result, star)?;
        }
        Ok(result)
    }

    fn concat(&mut self, left: Fragment, right: Fragment) -> Result<Fragment, Error> {
        self.patch(&left.outs, right.start);
        Ok(Fragment {
            start: left.start,
            outs: right.outs,
        })
    }

    fn alternate(&mut self, left: Fragment, right: Fragment) -> Result<Fragment, Error> {
        let mut split = Instruction::new(Op::Split);
        split.out = left.start;
        split.out1 = right.start;
        let split = self.add_instruction(split)?;
        let mut outs = Vec::with_capacity(left.outs.len() + right.outs.len());
        outs.extend_from_slice(&left.outs);
        outs.extend_from_slice(&right.outs);
        Ok(Fragment { start: split, outs })
    }

    fn optional(&mut self, child: Fragment) -> Result<Fragment, Error> {
        let mut split = Instruction::new(Op::Split);
        split.out = child.start;
        let split = self.add_instruction(split)?;
        let mut outs = Vec::with_capacity(child.outs.len() + 1);
        outs.extend_from_slice(&child.outs);
        outs.push(Patch {
            instruction: split,
            second: true,
        });
        Ok(Fragment { start: split, outs })
    }

    fn star(&mut self, child: Fragment) -> Result<Fragment, Error> {
        let mut split = Instruction::new(Op::Split);
        split.out = child.start;
        let split = self.add_instruction(split)?;
        self.patch(&child.outs, split);
        Ok(Fragment {
            start: split,
            outs: vec![Patch {
                instruction: split,
                second: true,
            }],
        })
    }

    fn single(&mut self, instruction: Instruction, second: bool) -> Result<Fragment, Error> {
        let index = self.add_instruction(instruction)?;
        Ok(Fragment {
            start: index,
            outs: vec![Patch {
                instruction: index,
                second,
            }],
        })
    }

    fn add_instruction(&mut self, instruction: Instruction) -> Result<usize, Error> {
        if self.instructions.len() >= self.limits.max_states {
            return Err(Error::PatternLimitExceeded);
        }
        let index = self.instructions.len();
        self.instructions.push(instruction);
        Ok(index)
    }

    fn patch(&mut self, patches: &[Patch], target: usize) {
        for item in patches {
            if item.second {
                self.instructions[item.instruction].out1 = target;
            } else {
                self.instructions[item.instruction].out = target;
            }
        }
    }
}

pub fn validate(source: &[u8], limits: Limits) -> Result<(), Error> {
    let mut parser = Parser::new(source, limits);
    let root = parser.parse()?;
    let mut compiler = Compiler::new(&parser.nodes, limits);
    compiler.compile(root)?;
    Ok(())
}

pub fn matches(
    source: &[u8],
    text: &[u8],
    limits: Limits,
    steps: &mut usize,
) -> Result<bool, Error> {
    let mut parser = Parser::new(source, limits);
    let root = parser.parse()?;
    let mut compiler = Compiler::new(&parser.nodes, limits);
    let start = compiler.compile(root)?;
    let instructions = std::mem::take(&mut compiler.instructions);
    drop(compiler);

    let mut current = BitSet::new(instructions.len());
    let mut next = BitSet::new(instructions.len());
    let mut visited = BitSet::new(instructions.len());
    let mut stack: Vec<usize> = Vec::new();

    let mut byte_index = 0usize;
    loop {
        visited.unset_all();
        add_closure(
            &instructions,
            &mut current,
            &mut visited,
            &mut stack,
            start,
            byte_index,
            text.len(),
            limits,
            steps,
        )?;
        if contains_accept(&instructions, &current) {
            return Ok(true);
        }
        if byte_index == text.len() {
            return Ok(false);
        }

        let (codepoint, width) = decode_utf8(&text[byte_index..]).map_err(|_| Error::InvalidUtf8)?;
        let next_byte_index = byte_index + width;
        next.unset_all();
        for instruction_index in 0..instructions.len() {
            if !current.is_set(instruction_index) {
                continue;
            }
            let instruction = instructions[instruction_index];
            let matched = match instruction.op {
                Op::Literal => codepoint as usize == instruction.value,
                Op::Any => !matches!(codepoint, '\n' | '\r' | '\u{2028}' | '\u{2029}'),
                Op::Class => parser.classes[instruction.value].matches(&parser.ranges, codepoint),
                _ => false,
            };
            if !matched {
                continue;
            }
            consume_step(limits, steps)?;
            visited.unset_all();
            add_closure(
                &instructions,
                &mut next,
                &mut visited,
                &mut stack,
                instruction.out,
                next_byte_index,
                text.len(),
                limits,
                steps,
            )?;
        }
        std::mem::swap(&mut current, &mut next);
        byte_index = next_byte_index;
    }
}

fn add_closure(
    instructions: &[Instruction],
    states: &mut BitSet,
    visited: &mut BitSet,
    stack: &mut Vec<usize>,
    start: usize,
    byte_index: usize,
    text_len: usize,
    limits: Limits,
    steps: &mut usize,
) -> Result<(), Error> {
    if start == INVALID_INDEX {
        return Err(Error::InvalidPattern);
    }
    stack.clear();
    stack.push(start);
    while let Some(index) = stack.pop() {
        if visited.is_set(index) {
            continue;
        }
        visited.set(index);
        consume_step(limits, steps)?;
        match instructions[index].op {
            Op::Epsilon => stack.push(instructions[index].out),
            Op::Split => {
                stack.push(instructions[index].out);
                stack.push(instructions[index].out1);
            }
            Op::AssertStart => {
                if byte_index == 0 {
                    stack.push(instructions[index].out);
                }
            }
            Op::AssertEnd => {
                if byte_index == text_len {
                    stack.push(instructions[index].out);
                }
            }
            _ => {
                states.set(index);
            }
        }
    }
    Ok(())
}

fn contains_accept(instructions: &[Instruction], states: &BitSet) -> bool {
    for index in 0..instructions.len() {
        if states.is_set(index) && matches!(instructions[index].op, Op::Accept) {
            return true;
        }
    }
    false
}

fn consume_step(limits: Limits, steps: &mut usize) -> Result<(), Error> {
    *steps = steps.checked_add(1).ok_or(Error::PatternLimitExceeded)?;
    if *steps > limits.max_steps {
        return Err(Error::PatternLimitExceeded);
    }
    Ok(())
}

fn decode_utf8(bytes: &[u8]) -> Result<(char, usize), ()> {
    let (&first, rest) = bytes.split_first().ok_or(())?;
    if first < 0x80 {
        return Ok((first as char, 1));
    }
    let (codepoint, width) = if (0xC2..=0xDF).contains(&first) {
        let b1 = *rest.get(0).ok_or(())?;
        if b1 & 0xC0 != 0x80 {
            return Err(());
        }
        ((((first & 0x1F) as u32) << 6) | ((b1 & 0x3F) as u32), 2)
    } else if (0xE0..=0xEF).contains(&first) {
        let b1 = *rest.get(0).ok_or(())?;
        let b2 = *rest.get(1).ok_or(())?;
        if b1 & 0xC0 != 0x80 || b2 & 0xC0 != 0x80 {
            return Err(());
        }
        let cp = (((first & 0x0F) as u32) << 12)
            | (((b1 & 0x3F) as u32) << 6)
            | ((b2 & 0x3F) as u32);
        if cp < 0x800 {
            return Err(());
        }
        if (0xD800..=0xDFFF).contains(&cp) {
            return Err(());
        }
        (cp, 3)
    } else if (0xF0..=0xF4).contains(&first) {
        let b1 = *rest.get(0).ok_or(())?;
        let b2 = *rest.get(1).ok_or(())?;
        let b3 = *rest.get(2).ok_or(())?;
        if b1 & 0xC0 != 0x80 || b2 & 0xC0 != 0x80 || b3 & 0xC0 != 0x80 {
            return Err(());
        }
        let cp = (((first & 0x07) as u32) << 18)
            | (((b1 & 0x3F) as u32) << 12)
            | (((b2 & 0x3F) as u32) << 6)
            | ((b3 & 0x3F) as u32);
        if cp < 0x10000 {
            return Err(());
        }
        if cp > 0x10FFFF {
            return Err(());
        }
        (cp, 4)
    } else {
        return Err(());
    };
    let ch = char::from_u32(codepoint).ok_or(())?;
    Ok((ch, width))
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

    fn unset_all(&mut self) {
        for word in &mut self.bits {
            *word = 0;
        }
    }

    fn set(&mut self, index: usize) {
        self.bits[index / 64] |= 1 << (index % 64);
    }

    fn is_set(&self, index: usize) -> bool {
        self.bits[index / 64] & (1 << (index % 64)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{is_ecmascript_whitespace, matches, validate, Error, Limits};

    fn limits() -> Limits {
        Limits {
            max_depth: 64,
            max_states: 256,
            max_repeat: 64,
            max_steps: 10_000,
        }
    }

    fn encoded(codepoint: char) -> String {
        codepoint.to_string()
    }

    #[test]
    fn bounded_pattern_matching_covers_the_json_schema_interoperability_subset() {
        let limits = limits();
        let cases: &[(&[u8], &[u8], bool)] = &[
            (b"^a*$", b"aaa", true),
            (b"^a*$", b"abc", false),
            (b"a+", b"xxaayy", true),
            (b"f.*o", b"foo", true),
            (b"[0-9]{2,}", b"v12", true),
            (b"^(ab|cd)?$", b"cd", true),
            (br"^\p{Letter}+$", "π".as_bytes(), true),
            (br"^\p{Letter}+$", b"123", false),
        ];
        for (pattern, text, expected) in cases {
            let mut steps = 0usize;
            assert_eq!(*expected, matches(pattern, text, limits, &mut steps).unwrap());
        }
    }

    #[test]
    fn ecmascript_whitespace_classification_covers_exact_codepoint_boundaries() {
        let whitespace = [
            '\u{0009}', '\u{000A}', '\u{000B}', '\u{000C}', '\u{000D}', '\u{0020}', '\u{00A0}',
            '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}',
            '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{2028}', '\u{2029}',
            '\u{202F}', '\u{205F}', '\u{3000}', '\u{FEFF}',
        ];
        for codepoint in whitespace {
            assert!(is_ecmascript_whitespace(codepoint));
        }

        let non_whitespace = [
            '\u{0008}', '\u{000E}', '\u{001F}', '\u{0021}', '\u{0085}', '\u{009F}', '\u{00A1}',
            '\u{167F}', '\u{1681}', '\u{180E}', '\u{1FFF}', '\u{200B}', '\u{2027}', '\u{202A}',
            '\u{202E}', '\u{2030}', '\u{205E}', '\u{2060}', '\u{2FFF}', '\u{3001}', '\u{FEFE}',
            '\u{FF00}',
        ];
        for codepoint in non_whitespace {
            assert!(!is_ecmascript_whitespace(codepoint));
        }
    }

    #[test]
    fn ecmascript_whitespace_escapes_share_exact_standalone_and_class_semantics() {
        let limits = limits();
        let whitespace = [
            '\u{0009}', '\u{000A}', '\u{000B}', '\u{000C}', '\u{000D}', '\u{0020}', '\u{00A0}',
            '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}',
            '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{2028}', '\u{2029}',
            '\u{202F}', '\u{205F}', '\u{3000}', '\u{FEFF}',
        ];
        let patterns: &[&[u8]] = &[
            br"^\s$",
            br"^\S$",
            br"^[\s]$",
            br"^[^\s]$",
            br"^[\S]$",
            br"^[^\S]$",
        ];
        let whitespace_results = [true, false, true, false, false, true];
        let non_whitespace_results = [false, true, false, true, true, false];

        for codepoint in whitespace {
            let text = encoded(codepoint);
            for (pattern, expected) in patterns.iter().zip(whitespace_results) {
                let mut steps = 0usize;
                assert_eq!(
                    expected,
                    matches(pattern, text.as_bytes(), limits, &mut steps).unwrap()
                );
            }
        }

        let non_whitespace = [
            '\u{0008}', '\u{000E}', '\u{0021}', '\u{0085}', '\u{180E}', '\u{200B}', '\u{202A}',
            '\u{2060}', '\u{3001}', '\u{FF00}',
        ];
        for codepoint in non_whitespace {
            let text = encoded(codepoint);
            for (pattern, expected) in patterns.iter().zip(non_whitespace_results) {
                let mut steps = 0usize;
                assert_eq!(
                    expected,
                    matches(pattern, text.as_bytes(), limits, &mut steps).unwrap()
                );
            }
        }

        let rejected = [
            "\u{00A0}", "\u{FEFF}", "\u{2003}", "\u{2028}", "\u{2029}",
        ];
        for text in rejected {
            let mut steps = 0usize;
            assert!(!matches(br"^\S+$", text.as_bytes(), limits, &mut steps).unwrap());
        }
    }

    #[test]
    fn pattern_compilation_and_matching_reject_unsupported_or_over_budget_behavior() {
        let limits = Limits {
            max_depth: 4,
            max_states: 32,
            max_repeat: 8,
            max_steps: 64,
        };
        assert!(matches!(validate(b"(?=a)", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(b"(a)\\1", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(br"^\cC$", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(br"^\cc$", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(br"^\p{digit}+$", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(br"\q", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(br"[\q]", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(br"\00", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(br"[\00]", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(b"]", limits), Err(Error::InvalidPattern)));
        assert!(matches!(validate(b"}", limits), Err(Error::InvalidPattern)));
        let sources: [&[u8]; 4] = [br"[\s-a]", br"[\w-a]", b"^*", b"$+"];
        for source in sources {
            assert!(matches!(validate(source, limits), Err(Error::InvalidPattern)));
        }
        validate(br"[\s-]", limits).unwrap();
        validate(b"(^)?", limits).unwrap();
        assert!(matches!(validate(b"a{9}", limits), Err(Error::PatternLimitExceeded)));
        validate(b"((((a))))", limits).unwrap();
        assert!(matches!(
            validate(b"(((((a)))))", limits),
            Err(Error::PatternLimitExceeded)
        ));
        let mut nul_steps = 0usize;
        assert!(matches(br"^[\0]$", &[0], limits, &mut nul_steps).unwrap());
        let mut steps = 0usize;
        assert!(matches!(
            matches(b"(a|aa)*b", b"aaaaaaaaaaaaaaaa", limits, &mut steps),
            Err(Error::PatternLimitExceeded)
        ));
    }
}
