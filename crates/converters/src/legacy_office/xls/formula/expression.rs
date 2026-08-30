//! A bounded RPN arena; each edge is visited once and no accumulated expression is cloned.

use super::reader::Result;

enum Node {
    Atom(String),
    Binary { left: usize, right: usize, operator: &'static str, precedence: u8 },
    Unary { child: usize, prefix: &'static str, suffix: &'static str, precedence: u8 },
    Call { name: &'static str, arguments: std::ops::Range<usize> },
}

impl Node {
    fn precedence(&self) -> u8 {
        match self {
            Self::Binary { precedence, .. } | Self::Unary { precedence, .. } => *precedence,
            Self::Atom(_) | Self::Call { .. } => 11,
        }
    }
}

#[derive(Default)]
pub(super) struct Expression {
    nodes: Vec<Node>,
    stack: Vec<usize>,
    arguments: Vec<usize>,
    bytes: usize,
}

enum Output<'a> {
    Node(usize, u8),
    Text(&'a str),
}

impl Expression {
    fn is_missing(&self, index: usize) -> bool {
        matches!(&self.nodes[index], Node::Atom(text) if text.is_empty())
    }

    fn push(&mut self, node: Node, bytes: usize) {
        self.stack.push(self.nodes.len());
        self.nodes.push(node);
        // Two parentheses per node suffice even when every edge needs grouping.
        self.bytes += bytes + 2;
    }

    pub(super) fn atom(&mut self, text: String) {
        let length = text.len();
        self.push(Node::Atom(text), length);
    }

    pub(super) fn binary(&mut self, token: u8) -> Result<()> {
        let right = self.stack.pop().ok_or("invalid-formula-stack")?;
        let left = self.stack.pop().ok_or("invalid-formula-stack")?;
        if self.is_missing(left) || self.is_missing(right) {
            return Err("missing-operator-operand");
        }
        let (operator, precedence) = match token {
            0x03 => ("+", 3),
            0x04 => ("-", 3),
            0x05 => ("*", 4),
            0x06 => ("/", 4),
            0x07 => ("^", 5),
            0x08 => ("&", 2),
            0x09 => ("<", 1),
            0x0a => ("<=", 1),
            0x0b => ("=", 1),
            0x0c => (">=", 1),
            0x0d => (">", 1),
            0x0e => ("<>", 1),
            // Reference union must not turn into a function argument separator.
            0x0f => (" ", 9),
            0x10 => (",", 8),
            0x11 => (":", 10),
            _ => return Err("unsupported-operator"),
        };
        self.push(Node::Binary { left, right, operator, precedence }, operator.len());
        Ok(())
    }

    pub(super) fn unary(&mut self, token: u8) -> Result<()> {
        let child = self.stack.pop().ok_or("invalid-formula-stack")?;
        if self.is_missing(child) {
            return Err("missing-operator-operand");
        }
        let (prefix, suffix, precedence) = match token {
            0x12 => ("+", "", 7),
            0x13 => ("-", "", 7),
            0x14 => ("", "%", 6),
            0x15 => ("(", ")", 11),
            _ => return Err("unsupported-operator"),
        };
        self.push(Node::Unary { child, prefix, suffix, precedence }, prefix.len() + suffix.len());
        Ok(())
    }

    pub(super) fn call(&mut self, name: &'static str, count: usize) -> Result<()> {
        let start = self.stack.len().checked_sub(count).ok_or("invalid-function-arguments")?;
        let first = self.arguments.len();
        self.arguments.extend(self.stack.drain(start..));
        self.push(
            Node::Call { name, arguments: first..self.arguments.len() },
            name.len() + 2 + count,
        );
        Ok(())
    }

    pub(super) fn capacity(&self) -> usize {
        self.bytes
    }

    pub(super) fn render(&self) -> Result<String> {
        if self.stack.len() != 1 || self.is_missing(self.stack[0]) {
            return Err("invalid-formula-stack");
        }
        let mut output = String::with_capacity(self.bytes);
        let mut pending = vec![Output::Node(self.stack[0], 0)];
        while let Some(item) = pending.pop() {
            match item {
                Output::Text(text) => output.push_str(text),
                Output::Node(index, parent) => {
                    let node = &self.nodes[index];
                    if node.precedence() < parent {
                        output.push('(');
                        pending.push(Output::Text(")"));
                    }
                    self.expand(node, &mut pending);
                }
            }
        }
        Ok(output)
    }

    fn expand<'a>(&'a self, node: &'a Node, pending: &mut Vec<Output<'a>>) {
        match node {
            Node::Atom(text) => pending.push(Output::Text(text)),
            Node::Binary { left, right, operator, precedence } => {
                pending.push(Output::Node(*right, precedence + 1));
                pending.push(Output::Text(operator));
                pending.push(Output::Node(*left, *precedence));
            }
            Node::Unary { child, prefix, suffix, precedence } => {
                pending.push(Output::Text(suffix));
                pending.push(Output::Node(*child, if *prefix == "(" { 0 } else { *precedence }));
                pending.push(Output::Text(prefix));
            }
            Node::Call { name, arguments } => {
                pending.push(Output::Text(")"));
                for (index, argument) in self.arguments[arguments.clone()].iter().enumerate().rev()
                {
                    let union = matches!(self.nodes[*argument], Node::Binary { operator: ",", .. });
                    pending.push(Output::Node(*argument, if union { 9 } else { 0 }));
                    if index != 0 {
                        pending.push(Output::Text(","));
                    }
                }
                pending.push(Output::Text("("));
                pending.push(Output::Text(name));
            }
        }
    }
}
