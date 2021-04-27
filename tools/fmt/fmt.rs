/* LICENSE BEGIN
This file is part of the SixtyFPS Project -- https://sixtyfps.io
Copyright (c) 2020 Olivier Goffart <olivier.goffart@sixtyfps.io>
Copyright (c) 2020 Simon Hausmann <simon.hausmann@sixtyfps.io>

    SPDX-License-Identifier: GPL-3.0-only
    This file is also available under commercial licensing terms.
    Please contact info@sixtyfps.io for more information.
    LICENSE END */

use crate::TokenWriter;
use sixtyfps_compilerlib::parser::{syntax_nodes, NodeOrToken, SyntaxKind, SyntaxNode};

pub(crate) fn format_document(
    doc: syntax_nodes::Document,
    writer: &mut impl TokenWriter,
) -> Result<(), std::io::Error> {
    format_node(&doc, writer, &mut FormatState::default())
}

#[derive(Default)]
struct FormatState {
    /// The whitespace have been writen, all further whitespace can be skipped
    skip_all_whitespace: bool,
    /// The whitespace to add before the next token
    whitespace_to_add: Option<String>,
    /// The level of indentation
    indentation_level: u32,
}

impl FormatState {
    fn new_line(&mut self) {
        self.skip_all_whitespace = true;
        if let Some(x) = &mut self.whitespace_to_add {
            x.insert_str(0, "\n");
            return;
        }
        let mut new_line = String::from("\n");
        for _ in 0..self.indentation_level {
            new_line += "    ";
        }
        self.whitespace_to_add = Some(new_line);
    }
}

fn format_node(
    node: &SyntaxNode,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
) -> Result<(), std::io::Error> {
    match node.kind() {
        SyntaxKind::Component => {
            return format_component(node, writer, state);
        }
        SyntaxKind::Element => {
            return format_element(node, writer, state);
        }
        SyntaxKind::PropertyDeclaration => {
            return format_property_declaration(node, writer, state);
        }
        SyntaxKind::Binding => {
            return format_binding(node, writer, state);
        }

        _ => (),
    }

    for n in node.children_with_tokens() {
        fold(n, writer, state)?;
    }
    Ok(())
}

fn fold(
    n: NodeOrToken,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
) -> std::io::Result<()> {
    match n {
        NodeOrToken::Node(n) => format_node(&n, writer, state),
        NodeOrToken::Token(t) => {
            if t.kind() == SyntaxKind::Whitespace {
                if state.skip_all_whitespace {
                    writer.with_new_content(t, "")?;
                    return Ok(());
                }
            } else {
                state.skip_all_whitespace = false;
                if let Some(x) = state.whitespace_to_add.take() {
                    writer.insert_before(t, x.as_ref())?;
                    return Ok(());
                }
            }
            writer.no_change(t)
        }
    }
}

fn whitespace_to(
    sub: &mut impl Iterator<Item = NodeOrToken>,
    element: SyntaxKind,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
    arg: &str,
) -> Result<bool, std::io::Error> {
    state.skip_all_whitespace = true;
    if !arg.is_empty() {
        if let Some(ws) = &mut state.whitespace_to_add {
            *ws += arg;
        } else {
            state.whitespace_to_add = Some(arg.into());
        }
    }

    for n in sub {
        match n.kind() {
            SyntaxKind::Whitespace | SyntaxKind::Comment => (),
            x if x == element => {
                fold(n, writer, state)?;
                return Ok(true);
            }
            _ => {
                eprintln!("Inconsistancy: expected {:?},  found {:?}", element, n);
                return Ok(false);
            }
        }
        fold(n, writer, state)?;
    }
    eprintln!("Inconsistancy: expected {:?},  not found", element);
    Ok(false)
}

fn finish_node(
    sub: impl Iterator<Item = NodeOrToken>,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
) -> Result<bool, std::io::Error> {
    // FIXME:  We should check that there are only comments or whitespace in sub
    for n in sub {
        fold(n, writer, state)?;
    }
    Ok(true)
}

fn format_component(
    node: &SyntaxNode,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
) -> Result<(), std::io::Error> {
    //let mut sub = node.first_child_or_token();
    let mut sub = node.children_with_tokens();
    let _ok = whitespace_to(&mut sub, SyntaxKind::DeclaredIdentifier, writer, state, "")?
        && whitespace_to(&mut sub, SyntaxKind::ColonEqual, writer, state, " ")?
        && whitespace_to(&mut sub, SyntaxKind::Element, writer, state, " ")?;

    finish_node(sub, writer, state)?;
    state.new_line();

    Ok(())
}

fn format_element(
    node: &SyntaxNode,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
) -> Result<(), std::io::Error> {
    let mut sub = node.children_with_tokens();
    if !(whitespace_to(&mut sub, SyntaxKind::QualifiedName, writer, state, "")?
        && whitespace_to(&mut sub, SyntaxKind::LBrace, writer, state, " ")?)
    {
        finish_node(sub, writer, state)?;
        return Ok(());
    }

    state.indentation_level += 1;
    state.new_line();

    for n in sub {
        if n.kind() == SyntaxKind::RBrace {
            state.indentation_level -= 1;
            state.whitespace_to_add = None;
            state.new_line();
            fold(n, writer, state)?;
            state.new_line();
        } else {
            fold(n, writer, state)?;
        }
    }
    Ok(())
}

fn format_property_declaration(
    node: &SyntaxNode,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
) -> Result<(), std::io::Error> {
    let mut sub = node.children_with_tokens();
    let _ok = whitespace_to(&mut sub, SyntaxKind::Identifier, writer, state, "")?
        && whitespace_to(&mut sub, SyntaxKind::LAngle, writer, state, " ")?
        && whitespace_to(&mut sub, SyntaxKind::Type, writer, state, "")?
        && whitespace_to(&mut sub, SyntaxKind::RAngle, writer, state, "")?
        && whitespace_to(&mut sub, SyntaxKind::DeclaredIdentifier, writer, state, " ")?;

    state.skip_all_whitespace = true;
    // FIXME: more formating
    for s in sub {
        fold(s, writer, state)?;
    }
    state.new_line();
    Ok(())
}

fn format_binding(
    node: &SyntaxNode,
    writer: &mut impl TokenWriter,
    state: &mut FormatState,
) -> Result<(), std::io::Error> {
    let mut sub = node.children_with_tokens();
    let _ok = whitespace_to(&mut sub, SyntaxKind::Identifier, writer, state, "")?
        && whitespace_to(&mut sub, SyntaxKind::Colon, writer, state, "")?
        && whitespace_to(&mut sub, SyntaxKind::BindingExpression, writer, state, " ")?;
    // FIXME: more formating
    for s in sub {
        fold(s, writer, state)?;
    }
    state.new_line();
    Ok(())
}
